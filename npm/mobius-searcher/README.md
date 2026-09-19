# mobius-searcher (npm)

Installs the prebuilt [MØBIUS](https://github.com/mangiapanejohn-dev/MOBIUS-Searcher)
binary for your platform (macOS arm64/x64, Linux x64/arm64, Windows x64).

```bash
npm install -g mobius-searcher
mobius-searcher --doctor
mobius-searcher
```

The package downloads `mobius-searcher-<target>` from the GitHub release of the
same version, checks it against the release's `SHA256SUMS`, and runs it. It
starts in PAPER mode; nothing is signed or sent. See the repository README for
configuration, keys and safety.
