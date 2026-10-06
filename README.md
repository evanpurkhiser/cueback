# Cueback

Cueback automatically records DJ performances from an XDJ-RX3. It preserves
the lossless master audio, the tracks that were heard, and a timeline of the
controls and deck state throughout the performance.

While the RX3 is available, Cueback listens for audio and keeps enough recent
data to capture the beginning of a performance. It records an active session
until the equipment has been idle, then processes that recording into one or
more useful sessions. Each resulting session can include a trimmed FLAC,
playlist, track timings, metadata, and a detailed event log.

The event log can also drive a synchronized replay showing what happened on
the decks and mixer while the recording plays. Replaying actions on physical
hardware may be explored later, but is not required for useful playback.

The project is currently in the design stage. See [DESIGN.md](DESIGN.md) for
the preliminary architecture and decisions.

## License

Cueback is available under the [MIT License](LICENSE).
