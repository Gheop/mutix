<p align="center"><img src="assets/icon.svg" alt="mutix icon: two AirPods shaped like a quotation mark, followed by an ellipsis" width="128"></p>

# mutix

Silent words to text on Linux, read from the motion sensors of your AirPods.

You mouth a word without a sound. Your jaw moves, the AirPods feel it, and mutix types the word into the focused window, presses a shortcut or runs a command. It works with a small vocabulary you train yourself in about 5 minutes, and it runs entirely on your machine. When you speak aloud, to someone in the room or in a call, the microphone notices it and nothing is typed.

> **Status: early prototype.** mutix connects, streams the motion sensors at about 85 Hz, records, recognizes and types. Recognition is still rough. On 4 French words (oui, non, bonjour, merci) trained in two short sessions, the offline test recognizes 74 % of the takes, where chance is 24 %. In a live test, mouthing the words freely, about half came out right. Expect a handful of distinct words, not dictation.

## Where it comes from

mutix is a Linux rewrite, in Rust, of [Subvox](https://github.com/joeblau/subvox) by Joe Blau, a macOS prototype that tests whether AirPods head motion carries enough information to recognize silently mouthed words. The recognizer follows the same idea: one template per take, dynamic time warping, and a permutation test to check that the signal is real.

macOS hands head motion to apps through `CMHeadphoneMotionManager`. Linux has nothing of the kind, so mutix talks to the AirPods directly over Bluetooth with the Apple Accessory Protocol, which [LibrePods](https://github.com/kavishdevar/librepods) reverse engineered. On top of that work, mutix found how to get the full motion stream from recent models and how to raise its rate from 25 to about 100 samples a second (see [Protocol notes](#protocol-notes)).

## Requirements

- Linux with BlueZ and systemd (mutix reads the device list with `busctl`), the AirPods paired and connected.
- AirPods with head tracking: AirPods Pro, Pro 2, Pro 3, AirPods 3 or 4, AirPods Max. Only AirPods Pro 3 have been tested.
- Rust 1.88 or later and `libxkbcommon` (`libxkbcommon-devel` on Fedora, `libxkbcommon-dev` on Debian and Ubuntu).
- For the training app, Rust 1.92 or later, GTK 4.12 and libadwaita 1.4 (`gtk4-devel libadwaita-devel` on Fedora, `libgtk-4-dev libadwaita-1-dev` on Debian and Ubuntu).
- `parec` (PipeWire or PulseAudio) to notice when you speak aloud.
- Write access to `/dev/uinput` to type, see below. Without it mutix still recognizes words and runs commands.

mutix shares the AirPods with other tools: BudsLink or LibrePods can stay connected.

## Install

```bash
./install.sh            # mutix, the Mutix training app, its launcher and icon
cargo install --path .  # or only the command line, without GTK
mutix doctor
```

`mutix doctor` checks each link in the chain:

```
ok   BlueZ: 2 AirPods paired, 1 connected
ok   Motion stream: 83 Hz (asked for 100 Hz)
ok   Microphone: listening (speaking aloud blocks typing)
ok   Keyboard layout: from GNOME
ok   Virtual keyboard (/dev/uinput)
```

If the virtual keyboard fails, give your session access to `/dev/uinput` with a udev rule, then log out and back in:

```bash
echo 'KERNEL=="uinput", SUBSYSTEM=="misc", TAG+="uaccess", OPTIONS+="static_node=uinput"' \
  | sudo tee /etc/udev/rules.d/70-mutix-uinput.rules
sudo udevadm control --reload && sudo udevadm trigger
```

Steam and Sunshine install the same rule, so it may already be there.

## Getting started

### With the Mutix app

Put both AirPods in and open **Mutix** from your applications (or run `mutix-gtk`). Choose the vocabulary, French or English, and start the training.

1. **Keep still** for 2 seconds: mutix measures the sensor noise.
2. **Karaoke.** A line of words appears. One word is underlined, then highlighted: mouth it without a sound. It turns green once its movement is recorded, red if it was skipped. If you said it aloud, or nothing moved, the app asks again. 12 words, 8 times each, take about 5 minutes.
3. **Results.** The share of takes recognized, against chance, and each word with the one it is most often mistaken for. Weak words can be trained again in one click.
4. **Test.** Mouth words freely: each one shows up green when recognized, grey when mutix is unsure, orange when you said it aloud.

Then run `mutix listen` to use the words everywhere.

### Built-in vocabularies

The words were picked to move the jaw and lips in clearly different ways, and each one does something useful out of the box:

| Français | English | Does |
|---|---|---|
| oui, non, bonjour, merci | yes, no, hello, thanks | types the word |
| entrée | enter | Return |
| efface | delete | Ctrl+Backspace (deletes a word) |
| annule | undo | Ctrl+Z |
| copie, colle | copy, paste | Ctrl+C, Ctrl+V |
| stop | stop | Escape |
| suivant | next | Tab |
| espace | space | Space |

Shortcuts go through your keyboard layout: on AZERTY, "annule" presses the key printed Z.

### From the command line

Put both AirPods in, then train a few words. With `--speak`, a voice tells you which word to mouth, take after take, so you can keep your eyes off the screen:

```bash
mutix train oui non bonjour merci --takes 10 --speak
```

Keep still for the first 2 seconds: mutix measures the sensor noise. Then, each time the voice says a word, mouth it without a sound. Each take is a 2.5-second window after the prompt; mutix keeps only the part that moves.

Check that the AirPods tell your words apart:

```bash
mutix eval
```

```
attitude+rotation+acceleration: 72 takes, 4 words. Accuracy 74 %, chance 24 %, p = 0.002  → the motion tells these words apart

 said ↓ bonjour   merci     non     oui
bonjour      14       1       1       2
  merci       2      15       1       0
    non       2       1      13       2
    oui       5       1       1      11
```

The accuracy comes from leaving each take out in turn and recognizing it with the others. The p-value compares it with the same test on shuffled labels: below 0.05, the motion really carries your words. `mutix eval --compare` tries every combination of sensor groups.

Try it without typing anything, with the recognized word said back in your ears:

```bash
mutix listen --dry-run --speak
```

Then for real: `mutix listen` acts on each recognized word in the focused window.

### Speaking aloud

mutix listens to the microphone, by default the computer's own, and keeps only a loudness level every 20 ms; no audio is stored. When a word was mouthed while you were speaking aloud, it is ignored: you can talk with someone, or in a call, without mutix typing. A colleague talking near you has the same effect, which errs on the safe side. Avoid choosing the AirPods' microphone: it switches them to the low-quality headset profile. `--no-voice-guard` or `voice_guard = false` turns this off.

## Commands

| Command | What it does |
|---|---|
| `mutix` or `mutix ui` | Terminal interface: live sensor plots, training, evaluation and decoding |
| `mutix doctor` | Check Bluetooth, the motion stream, the keyboard layout and `/dev/uinput` |
| `mutix devices` | List the AirPods BlueZ knows |
| `mutix stream [--csv] [--seconds N]` | Print motion samples; `--csv` to record them |
| `mutix train WORD... [--takes N] [--speak]` | Record takes of one or more words |
| `mutix words` | List trained words and their number of takes |
| `mutix forget WORD [--last]` | Delete a word, or only its last take |
| `mutix eval [--compare]` | Leave-one-out accuracy, permutation test, confusion matrix |
| `mutix-gtk` | The training app: karaoke, results and live test |
| `mutix listen [--dry-run] [--manual] [--speak] [--no-voice-guard]` | Recognize words and act on them |
| `mutix toggle` | Start or end a capture in a running `mutix listen --manual` |
| `mutix calibrate` | Make a running `mutix listen` measure the noise again |
| `mutix config [--init]` | Show the configuration path, or write a commented sample |

### Terminal interface

`mutix` opens three screens; Tab or 1, 2, 3 switch between them.

- **Sensors**: attitude, rotation rate, user acceleration and gravity over the last 5 seconds, and the motion energy against the trigger threshold.
- **Train**: `n` adds a word, ↑↓ choose it. Space captures a take (hold it in terminals with the kitty keyboard protocol, such as kitty, foot, WezTerm or Ghostty; elsewhere press once to start and once to stop). `a` captures every movement as a take of the selected word. `x` deletes the last take, `X` the whole word. `e` evaluates; `o`, `r`, `l`, `g` toggle the attitude, rotation, linear acceleration and gravity groups.
- **Decode**: Space or `a` (auto) as above, with each result, its distance and its margin.

`c` measures the still noise again, `q` quits.

### A keyboard shortcut instead of auto detection

Auto detection reacts to any jaw or head movement; rejected ones are ignored, but a shortcut is more reliable. Start `mutix listen --manual`, then in GNOME Settings › Keyboard › Custom Shortcuts, bind a key to `mutix toggle`: press it, mouth the word, press it again.

### Run at login

```ini
# ~/.config/systemd/user/mutix.service
[Unit]
Description=mutix silent word recognition

[Service]
ExecStart=%h/.cargo/bin/mutix listen
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user enable --now mutix
```

## Configuration

`mutix config --init` writes `~/.config/mutix/config.toml` with every key commented. The main ones:

```toml
device = "A4:C3:F0:12:34:56"   # default: the first connected AirPods
interval_ms = 10               # 5 to 40; 10 gives about 85-100 Hz
features = ["attitude", "rotation", "acceleration"]
trigger = 3.0                  # auto detection: start at 3× the noise, end below half of that
keyboard = "fr"                # default: GNOME's first input source
microphone = "alsa_input.pci-0000_00_1f.3.analog-stereo"   # default: the default input
voice_guard = true             # ignore words mouthed while you speak aloud
speak = "spd-say -w \"$MUTIX_TEXT\""   # command for --speak; the text also comes on stdin

# What each word does. Built-in words have a default; any other word is
# typed followed by a space.
[words.bonjour]
text = "Bonjour !\n"

[words.annule]
keys = "ctrl+z"                # xkb key names: Return, Escape, BackSpace, a letter…

[words.suivant]
command = "playerctl next"     # $MUTIX_WORD holds the word
```

Takes are stored in `~/.local/share/mutix/dataset.json`, as raw samples: changing the features never requires recording again.

## How it works

1. **Connection.** mutix finds the AirPods through BlueZ, opens an L2CAP channel on PSM 0x1001 with medium security, and sends the AAP handshake.
2. **Motion stream.** It asks for stream 14, then stream 16 every 10 ms, then stops stream 14. Each sample carries a timestamp in nanoseconds, the attitude, the rotation rate, the user acceleration and gravity, as three signed 16-bit values each.
3. **Segmentation.** The motion energy is the size of the motion vector (attitude change, rotation rate, user acceleration) in units of the sensor noise. The noise is measured on a still head, then follows slow changes while nobody speaks. In auto mode, a word starts above 3 times the noise and ends after 0.3 s below half of that.
4. **Features.** mutix keeps the frames around the strongest movement, smooths each channel, takes its derivative, and scales the take to a fixed overall amplitude, so a word mouthed more or less widely keeps its shape.
5. **Matching.** Each channel is scaled by its spread over all takes. A new segment is compared with every take by dynamic time warping, within a band of 15 % of its length. It gets the word of the nearest take, unless it is too far from all of them (the limit is learned from your takes) or too close to two words.

### Protocol notes

What mutix adds to what LibrePods documents, measured on AirPods Pro 3:

- The L2CAP channel needs at least medium security (`BT_SECURITY_MEDIUM`). With the default level the AirPods accept the connection and never answer.
- The head-tracking request (opcode 0x17) is a protobuf message: `08 <seq varint> 42 0b { 08 <stream> 10 02 1a 05 01 <interval µs, u32 LE> }`. An interval of 0 stops the stream.
- Stream 16 carries the motion samples; stream 14 answers with small status messages. Stream 16 only starts once stream 14 has been requested; stopping stream 14 afterwards leaves stream 16 running.
- The AirPods honour the interval: 40 000 µs gives 25 Hz, 10 000 µs about 100 Hz. At 5 000 µs Bluetooth delivers about 90 samples a second, no more than at 10 ms.
- The sample is the last field of the reply, a 58-byte blob: byte 0 = 1, bytes 1-8 = timestamp (ns), 20-25 attitude, 26-31 rotation rate, 38-43 user acceleration, 44-49 gravity (about 1024 per g). The first samples after a start have a zero attitude.

## Limits

- Each user trains their own words; takes from another person, or from the AirPods seated differently, match less well.
- The sensors feel the jaw and the head, not the voice: a head movement in the middle of a word blurs it.
- The motion stream pauses when a bud leaves the ear and comes back by itself.
- Typing goes through a virtual keyboard, so it works in every Wayland and X11 application. Characters missing from the layout go through Ctrl+Shift+U, which GTK and IBus understand.

## License

MIT

## README changelog

| Version | Date       | Changes                                          |
|---------|------------|--------------------------------------------------|
| 1.1.0   | 2026-10-31 | Add the Mutix app, vocabularies and voice guard  |
| 1.0.1   | 2026-10-31 | Replace the icon with the quotation-mark AirPods |
| 1.0.0   | 2026-10-31 | Write the first version of the README            |
