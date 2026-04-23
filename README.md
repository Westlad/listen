# openclaw-listen

`openclaw-listen` is a command-line Rust application for Linux that listens to a local microphone, transcribes speech with OpenAI Whisper, and sends the resulting text to an OpenClaw Gateway session.

## Goals

- Run as a local CLI daemon on Linux.
- Capture microphone input from a local sound device.
- Detect speech boundaries automatically using a simple silence gate.
- Transcribe utterances with the OpenAI audio transcription API.
- Send the transcribed text into an OpenClaw session through the Gateway.

## Configuration

Configuration is loaded from:

1. `--config <path>` if provided
2. `$XDG_CONFIG_HOME/openclaw-listen/config.toml`
3. `~/.config/openclaw-listen/config.toml`
4. `.env` in the current working directory, if present
5. environment variables

Environment variables:

- `OPENCLAW_GATEWAY_URL`
- `OPENCLAW_GATEWAY_TOKEN`
- `OPENCLAW_SESSION_KEY`
- `OPENCLAW_SESSION_FILTER`
- `OPENAI_API_KEY`
- `OPENAI_BASE_URL`
- `OPENAI_TRANSCRIPTION_MODEL`
- `OPENAI_TRANSCRIPTION_LANGUAGE`
- `OPENAI_TRANSCRIPTION_PROMPT`
- `AUDIO_INPUT_DEVICE`
- `OPENCLAW_LISTEN_LOG_PATH`
- `RUST_LOG`

See [`config.example.toml`](./config.example.toml).
For secret values, see [`.env.example`](./.env.example).

Recommended split:

- keep stable non-secret settings in `config.toml`
- keep secrets such as `OPENCLAW_GATEWAY_TOKEN` and `OPENAI_API_KEY` in `.env`
- use real exported shell env vars only when you want to override `.env`

Example `.env`:

```bash
OPENCLAW_GATEWAY_TOKEN=replace-me
OPENAI_API_KEY=replace-me
```

## Target Session Selection

`openclaw-listen` needs a single destination session for outgoing transcripts.

- Set `openclaw.session_key` or `OPENCLAW_SESSION_KEY` for an exact target.
- Otherwise, set `openclaw.session_filter` or `OPENCLAW_SESSION_FILTER` and the app will require that it matches exactly one live session.
- If neither is set, the app will try a `main` session first. If that is still ambiguous, it will ask you to configure a target explicitly.

## Development

```bash
cargo run -- sessions
cargo run -- test
cargo run -- test --send
cargo run -- daemon
```

To enable real Linux microphone access through CPAL and ALSA:

```bash
cargo run --features audio-cpal -- devices
cargo run --features audio-cpal -- test
cargo run --features audio-cpal -- daemon
```

On Debian or Ubuntu style systems, that feature typically needs:

```bash
sudo apt install pkg-config libasound2-dev
```

With `audio-cpal` enabled, the app will:

- listen to the configured microphone
- wait for speech to cross the configured amplitude threshold
- stop after trailing silence
- resample to mono 16 kHz WAV
- send the captured utterance to OpenAI for transcription
- forward the resulting text to OpenClaw using `chat.send`
- append transcribed speech and observed agent replies to `/var/log/openclaw-listen.log`

## Transcript Log

When running `daemon` or `test --send`, `openclaw-listen` appends JSON Lines entries to the configured transcript log.
The default path is `/var/log/openclaw-listen.log`; override it with `OPENCLAW_LISTEN_LOG_PATH` or `[logging].transcript_log_path`.

Example entry:

```json
{"timestamp_unix_ms":1776935117036,"session_key":"agent:main:telegram:direct:8735858952","role":"user","text":"Good morning."}
```

## systemd

Build the release binary and install the bundled service and tmpfiles config:

```bash
cargo build --release --features audio-cpal
sudo install -m 0644 systemd/openclaw-listen.service /etc/systemd/system/openclaw-listen.service
sudo install -m 0644 systemd/openclaw-listen.tmpfiles /etc/tmpfiles.d/openclaw-listen.conf
sudo systemd-tmpfiles --create /etc/tmpfiles.d/openclaw-listen.conf
sudo systemctl daemon-reload
sudo systemctl enable --now openclaw-listen.service
```

The included unit expects this checkout at `/home/duncan/git/listen`, reads credentials from `/home/duncan/git/listen/.env`, and runs as user `duncan`.
