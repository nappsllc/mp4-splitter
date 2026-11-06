# Changelog

## Unreleased

- **Updates:** once a day the app asks GitHub whether there is a newer release, and offers it with its notes. The Windows exe downloads it, checks it against the release's `SHA256SUMS.txt`, swaps itself and restarts when you say so; a new version that won't start is swapped back. macOS and Linux open the download page. Turn the daily check off in Settings; **Check for updates** is in Settings and About. The request says only which app and version is asking.
- **Microsoft Store package:** each release can build a package for the Store, which then handles its updates. Not submitted yet.

## 0.1.1

- The app is now called **PowerNapps MP4 Splitter**. Nothing else changes: the file names, your settings and links stay as they were.
- The known limits of 0.1.0 still apply: the builds are unsigned, so Windows or macOS may warn; macOS and Linux need ffmpeg for pictures and sound and don't offer seven of the languages yet; translations are machine-made.

## 0.1.0

The first version of MP4 Splitter: cut a video into pieces without re-encoding.

- **Lossless trim and split**, exact to the frame, with cuts in the middle and presets for 2, 3, 4, 5 or any number of equal pieces.
- **Remove the gaps:** switch pieces off, then export the rest as separate files or joined into one.
- **A timeline** with thumbnails, the sound's waveform, key-frame marks and zoom; playback with sound; frame-by-frame stepping from the keyboard.
- **Window sizing:** the video's actual size, or the window fitted to the video.
- **35 languages**, right to left where they read that way, and dark and light themes with accent colours.
- **Windows:** hand-rolled on the system's own APIs, with GPU decoding through Media Foundation. About 0.7 MB, with nothing else to install.

### Good to know

- **The builds are not signed.** Windows SmartScreen or Defender may warn about the exe, and may flag it, and macOS Gatekeeper may refuse to open the app until you allow it. Each file's SHA-256 is in `SHA256SUMS.txt`.
- **macOS and Linux need [ffmpeg](https://ffmpeg.org/download.html) installed** for pictures and sound in the window (`brew install ffmpeg`, `sudo apt install ffmpeg`). Cutting works without it.
- **macOS and Linux can't draw Arabic, Hebrew, Hindi, Thai, Chinese, Japanese or Korean yet**, so those languages aren't offered there.
- **Translations are machine-made** and haven't been reviewed by native speakers. Corrections are welcome.
- **Joined files and players that ignore edit lists:** a joined piece starts at its key frame and an edit list hides the extra frames, so those players can show them. Snap to key frames (N) avoids this.
- **H.265 on Windows** needs Microsoft's HEVC extension.
