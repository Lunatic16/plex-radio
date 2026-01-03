# Plex Radio (Rust)

A lightweight, high-performance web radio player for your Plex Media Server, written in Rust using Axum and Tokio.

## Features

- **Continuous Radio Stream**: Plays a continuous stream of music from your Plex library.
- **Web Interface**: Clean, responsive UI with:
  - Real-time "Now Playing" metadata.
  - Audio Visualizer.
  - Playback controls (Play, Pause, Skip, Shuffle).
  - Volume control.
  - Search functionality to queue specific tracks.
  - Recently played history.
- **Transcoding Support**: Uses Plex's universal transcoder to normalize audio and ensure compatibility.
- **Passthrough Mode**: Optional direct streaming for local network performance.
- **Auto-Discovery**: Automatically detects the first Music library on your Plex server.

## Prerequisites

- Rust (Stable toolchain recommended)
- A running Plex Media Server
- A Plex Token (`X-Plex-Token`)

## Setup & Installation

1. **Clone the repository**

2. **Configure Environment**
   Create a `.env` file in the project root:

   ```env
   PLEX_URL=http://your-plex-ip:32400
   PLEX_TOKEN=your-plex-token
   
   # Optional Settings
   PORT=3000
   PLEX_BITRATE=320
   PLEX_AUDIO_BOOST=100
   PLEX_PASSTHROUGH=true # Set to false to enable transcoding
   # PLEX_SECTION_ID=1  # Uncomment to force a specific library ID
   ```

3. **Run the Application**
   ```bash
   cargo run --release
   ```

4. **Access the Radio**
   Open `http://localhost:3000` in your web browser.

## Configuration Reference

| Variable | Description | Default |
|----------|-------------|---------|
| `PLEX_URL` | Base URL of your Plex Server | Required |
| `PLEX_TOKEN` | Plex Authentication Token | Required |
| `PORT` | Web server port | `3000` |
| `PLEX_BITRATE` | Max bitrate (kbps) for transcoding | `320` |
| `PLEX_AUDIO_BOOST` | Audio volume boost % | `100` |
| `PLEX_PASSTHROUGH` | Direct stream without transcoding | `true` |
| `PLEX_SECTION_ID` | Specific Library ID to scan | Auto-detected |
