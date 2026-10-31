//! Finds AirPods known to BlueZ. It asks `busctl` for the object tree rather
//! than pulling in a D-Bus stack: one call at start-up is all mutix needs.

use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// Service UUID of the Apple Accessory Protocol, advertised by AirPods.
const AAP_UUID: &str = "74ec2172-0bad-4d01-8f77-997b2be0722a";

#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub address: String,
    pub name: String,
    pub connected: bool,
}

pub fn devices() -> Result<Vec<Device>> {
    let out = Command::new("busctl")
        .args([
            "--system",
            "--json=short",
            "call",
            "org.bluez",
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "GetManagedObjects",
        ])
        .output()
        .context("cannot run busctl; is systemd installed?")?;
    if !out.status.success() {
        bail!(
            "cannot read BlueZ devices: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse(&out.stdout)
}

fn parse(json: &[u8]) -> Result<Vec<Device>> {
    let root: Value = serde_json::from_slice(json).context("unexpected busctl output")?;
    let objects = root["data"][0].as_object().context("unexpected busctl output")?;
    let mut found = Vec::new();
    for ifaces in objects.values() {
        let dev = &ifaces["org.bluez.Device1"];
        if dev.is_null() {
            continue;
        }
        let has_aap = dev["UUIDs"]["data"].as_array().is_some_and(|uuids| {
            uuids
                .iter()
                .any(|u| u.as_str().is_some_and(|u| u.eq_ignore_ascii_case(AAP_UUID)))
        });
        if !has_aap {
            continue;
        }
        found.push(Device {
            address: dev["Address"]["data"].as_str().unwrap_or_default().to_string(),
            name: dev["Alias"]["data"]
                .as_str()
                .or(dev["Name"]["data"].as_str())
                .unwrap_or("AirPods")
                .to_string(),
            connected: dev["Connected"]["data"].as_bool().unwrap_or(false),
        });
    }
    found.sort_by_key(|d| !d.connected);
    Ok(found)
}

/// The device to use: the configured one, or the first connected AirPods.
pub fn pick(wanted: Option<&str>) -> Result<Device> {
    let all = devices()?;
    if let Some(w) = wanted {
        // An address BlueZ does not list is still worth a try.
        let fallback = Device {
            address: w.to_string(),
            name: w.to_string(),
            connected: true,
        };
        return Ok(all
            .into_iter()
            .find(|d| d.address.eq_ignore_ascii_case(w) || d.name == w)
            .unwrap_or(fallback));
    }
    match all.into_iter().find(|d| d.connected) {
        Some(d) => Ok(d),
        None => bail!("no AirPods connected; connect them in the Bluetooth settings first"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_airpods_connected_first() {
        let json = br#"{"type":"a{oa{sa{sv}}}","data":[{
            "/org/bluez/hci0":{"org.bluez.Adapter1":{}},
            "/org/bluez/hci0/dev_10":{"org.bluez.Device1":{"Address":{"type":"s","data":"A4:C3:F0:65:43:21"},"Alias":{"type":"s","data":"AirPods Pro"},"Connected":{"type":"b","data":false},"UUIDs":{"type":"as","data":["74ec2172-0bad-4d01-8f77-997b2be0722a"]}}},
            "/org/bluez/hci0/dev_E4":{"org.bluez.Device1":{"Address":{"type":"s","data":"00:1A:7D:DA:71:13"},"Alias":{"type":"s","data":"Gamepad"},"Connected":{"type":"b","data":true},"UUIDs":{"type":"as","data":["00001124-0000-1000-8000-00805f9b34fb"]}}},
            "/org/bluez/hci0/dev_34":{"org.bluez.Device1":{"Address":{"type":"s","data":"A4:C3:F0:12:34:56"},"Alias":{"type":"s","data":"AirPods Pro 3"},"Connected":{"type":"b","data":true},"UUIDs":{"type":"as","data":["74EC2172-0BAD-4D01-8F77-997B2BE0722A"]}}}
        }]}"#;
        let found = parse(json).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "AirPods Pro 3");
        assert!(found[0].connected);
        assert!(!found[1].connected);
    }
}
