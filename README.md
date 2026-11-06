<h1 align="center">MP4 Splitter</h1>

**A tiny, good-looking, cross-platform tool that cuts MP4 videos into pieces
without re-encoding.** Windows, macOS and Linux. Apache-2.0.

![MP4 Splitter in the dark theme: a video trimmed at the start and cut into two pieces](docs/splitter-dark.png)

## Features

- **Lossless.** Pieces are copied sample by sample from the original, never
  re-encoded, so they keep the original quality and are written in about the
  time it takes to copy the file. Cuts are exact to the frame: each piece
  starts at the key frame before the cut, and an edit list makes it begin
  exactly where you cut (ffmpeg, VLC, browsers and QuickTime honour it).
  Turn on *Snap to key frames* (<kbd>N</kbd>) for pieces that start on a key
  frame for players that ignore edit lists.
- **Trim the start and end** with the handles at both ends of the timeline,
  or <kbd>I</kbd> / <kbd>O</kbd> at the playhead.
- **Cut in the middle**: <kbd>S</kbd> at the playhead, or double-click the
  timeline. Drag a cut to move it; right-click or double-click it to remove it.
- **Presets**: split the kept part into 2, 3, 4, 5 or any number of equal
  pieces (the buttons, or <kbd>2</kbd>–<kbd>9</kbd>).
- **A timeline like YouTube Studio's**: thumbnails, the sound's waveform, a
  ruler with key-frame marks, and the pieces with their lengths. Zoom with the
  mouse wheel (down to single frames), pan with Shift + wheel or the middle
  button.
- **Playback with sound**, scrubbing with the mouse and frame-exact stepping
  with the keyboard.
- **Dark and light themes** that follow your OS, with Rustshot's design
  tokens and the Inter font. Everything is drawn by the app itself (no GUI
  toolkit, no web view); the release build is about 5 MB.

![The light theme](docs/splitter-light.png)

## Keys

| Key | Does |
|---|---|
| <kbd>Space</kbd> / <kbd>K</kbd> | Play / pause |
| <kbd>←</kbd> <kbd>→</kbd> (or <kbd>,</kbd> <kbd>.</kbd>, or the wheel over the picture) | Previous / next frame |
| <kbd>Shift</kbd>+<kbd>←</kbd> <kbd>→</kbd>, <kbd>J</kbd> / <kbd>L</kbd> | 1 second back / forward |
| <kbd>Ctrl</kbd>+<kbd>←</kbd> <kbd>→</kbd> | 5 seconds back / forward |
| <kbd>↑</kbd> <kbd>↓</kbd> | Previous / next marker (start, cuts, end) |
| <kbd>PgUp</kbd> <kbd>PgDn</kbd> | Previous / next key frame |
| <kbd>Home</kbd> <kbd>End</kbd> | Start / end of the video |
| <kbd>I</kbd> or <kbd>[</kbd>, <kbd>O</kbd> or <kbd>]</kbd> | Set the start / end at the playhead |
| <kbd>S</kbd> (or <kbd>C</kbd>, <kbd>X</kbd>) | Cut at the playhead |
| <kbd>Delete</kbd> | Remove the cut at the playhead |
| <kbd>2</kbd>–<kbd>9</kbd>, <kbd>0</kbd> | Split into N equal pieces; remove all cuts |
| <kbd>N</kbd> | Snap to key frames |
| <kbd>M</kbd> | Mute |
| <kbd>+</kbd> <kbd>-</kbd> <kbd>\</kbd> | Zoom in / out / show everything |
| <kbd>Ctrl</kbd>+<kbd>O</kbd> | Open (or drop a file on the window) |
| <kbd>Ctrl</kbd>+<kbd>E</kbd> | Export next to the video (<kbd>Shift</kbd>+click Export: choose a folder) |

Pieces are saved as `<name>_part01.mp4`, `<name>_part02.mp4`, … (or
`<name>_trimmed.mp4` for one piece) and never overwrite a file.

## ffmpeg

Cutting is done by MP4 Splitter itself. The **pictures and sound** in the
window come from [ffmpeg](https://ffmpeg.org/download.html), which must be
installed: `winget install ffmpeg` (Windows), `brew install ffmpeg` (macOS),
`sudo apt install ffmpeg` (Debian, Ubuntu). It is looked for next to the app,
on `PATH`, in Homebrew's folders, or at `MP4_SPLITTER_FFMPEG`. Without it the
app still trims and splits, without previews.

## Command line

```
mp4-splitter [VIDEO]                    open the window
mp4-splitter --split 3 VIDEO            three equal pieces, no window
mp4-splitter --cut 12.5,40 VIDEO        cut at 12.5 s and 40 s
     [--start S] [--end E]              keep only S..E (seconds)
mp4-splitter --info VIDEO               frames, key frames, size, codecs
```

## Building

Rust 1.85 or newer (edition 2024). Linux also needs the ALSA headers
(`libasound2-dev`) to build and `libxkbcommon-x11-0` to run.

```
cargo build --release
cargo test        # the cutting tests use ffmpeg to make and check files
```

The cutting code (`src/mp4.rs`) is adapted from
[Rustshot](https://github.com/nappsllc/rustshot)'s trim. Files must be plain
(not fragmented) MP4, M4V or MOV; fragmented files are refused with a message.

## License

[Apache License 2.0](LICENSE); see [NOTICE](NOTICE). The Inter font is under
the SIL Open Font License 1.1.
