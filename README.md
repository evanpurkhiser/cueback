# Cueback

[![Build Status](https://github.com/evanpurkhiser/cueback/actions/workflows/ci.yml/badge.svg)](https://github.com/evanpurkhiser/cueback/actions/workflows/ci.yml)

Cueback automatically records DJ performances from an XDJ-RX3. It preserves
the lossless master audio, the tracks that were heard, and a timeline of the
controls and deck state throughout the performance.

> [!NOTE]
> Cueback is currently designed specifically for an XDJ-RX3 running the
> [`rx3-toolkit` PCM streaming firmware](https://github.com/evanpurkhiser/rx3-toolkit/pull/9)
> and, for wireless networking,
> [native USB Wi-Fi support](https://github.com/evanpurkhiser/rx3-toolkit/pull/14).
> An Ethernet module can provide networking instead. A future version will use
> [RX3 remote control](https://github.com/evanpurkhiser/rx3-toolkit/pull/15)
> to capture physical control actions for the timeline log.

While the RX3 is available, Cueback listens to its live PCM output. The first
audible frame starts a recording, and a configurable period of continuous
silence ends it. Each recording is streamed directly to a lossless FLAC. Later
processing can turn that capture into one or more useful sessions containing a
trimmed FLAC, playlist, track timings, metadata, and a detailed event log.

The event log can also drive a synchronized replay showing what happened on
the decks and mixer while the recording plays. Replaying actions on physical
hardware may be explored later, but is not required for useful playback.

The first live-session recorder is under development. See
[DESIGN.md](DESIGN.md) for the architecture and planned processing stages.

## Development

Install the project tools and create a local configuration:

```console
$ mise install
$ cp cueback.example.toml cueback.toml
$ cargo run
```

Cueback discovers the RX3 from its PRO DJ LINK announcements. The
configuration defines the recordings directory, RX3 connection behavior, and
live-session timing. A relative recordings path is resolved from the
configuration file's directory. Use `--config` to select a file other than
`cueback.toml`.

Active files remain under `recordings/.live`. A completed recording is promoted
to `recordings/session-YYYY-MM-DD-HHMM-<short-id>/master.flac`.

Run the repository checks with `prek run --all-files`.

## License

Cueback is available under the [MIT License](LICENSE).
