# Privacy

PowerNapps MP4 Splitter works on your computer. It collects nothing, has no accounts, no
analytics and no ads, and your videos never leave your machine.

## What goes over the network

**The update check**, only in the version downloaded from GitHub Releases: once a day it asks
`api.github.com` whether there's a newer release, and if you choose to update, it downloads
it from GitHub. The request says which app and version is asking
(`mp4-splitter/<version>`) and contains nothing about you or your files. GitHub sees your IP
address, as with any request; see
[GitHub's privacy statement](https://docs.github.com/site-policy/privacy-policies/github-general-privacy-statement).
Turn the daily check off in Settings. On macOS and Linux the check goes through the system's
`curl`.

**The Microsoft Store version** makes no requests at all: the Store handles its updates.

**Links** such as powernapps.net in About open in your browser, and only when you click them.

## What stays on your computer

- Your settings, and the date of the last update check, in `%APPDATA%\mp4-splitter` on Windows,
  `~/Library/Application Support/mp4-splitter` on macOS and `~/.config/mp4-splitter` on Linux.
- A downloaded update, briefly, in `%LOCALAPPDATA%\mp4-splitter\update`.

## Contact

Questions: open an issue at <https://github.com/nappsllc/mp4-splitter/issues>.
