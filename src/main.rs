use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Json, Response},
    routing::get,
    Router,
};
use bytes::Bytes;
use futures::Stream;
use rand::Rng;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::{net::SocketAddr, sync::Arc, time::{Duration, SystemTime}};
use tracing::{error, info, warn};

// --- Configuration & State ---

#[derive(Clone)]
struct AppState {
    client: Client,
    plex_url: String,
    plex_token: String,
    // We cache track keys to avoid hitting the DB for every song
    tracks: Arc<Vec<Track>>,
    // Map session_id -> Current Track
    sessions: Arc<std::sync::Mutex<HashMap<String, (Track, SystemTime)>>>,
    // Map client_id -> History (Recent Tracks)
    history: Arc<std::sync::Mutex<HashMap<String, Vec<Track>>>>,
    bitrate: u32,
    audio_boost: u32,
    passthrough: bool,
}

#[derive(Clone, Debug, Serialize)]
struct Track {
    key: String,
    title: String,
    artist: String,
    duration: u64,
}

// --- Plex API Models ---

#[derive(Deserialize, Debug)]
struct PlexContainer {
    #[serde(rename = "MediaContainer")]
    media_container: MediaContainer,
}

#[derive(Deserialize, Debug)]
struct MediaContainer {
    #[serde(rename = "Metadata", default)]
    metadata: Vec<PlexMetadata>,
    #[serde(rename = "Directory", default)]
    directories: Vec<PlexDirectory>,
}

#[derive(Deserialize, Debug)]
struct PlexMetadata {
    #[serde(rename = "ratingKey")]
    rating_key: String,
    title: String,
    #[serde(rename = "grandparentTitle", default)]
    artist: String,
    #[serde(default)]
    duration: u64,
}

#[derive(Deserialize, Debug)]
struct PlexDirectory {
    key: String,
    #[serde(rename = "type")]
    section_type: String,
    title: String,
}

// For fetching track details (Passthrough mode)
#[derive(Deserialize, Debug)]
struct TrackContainer {
    #[serde(rename = "MediaContainer")]
    media_container: TrackMediaContainer,
}

#[derive(Deserialize, Debug)]
struct TrackMediaContainer {
    #[serde(rename = "Metadata")]
    metadata: Vec<TrackMetadata>,
}

#[derive(Deserialize, Debug)]
struct TrackMetadata {
    #[serde(rename = "Media")]
    media: Vec<TrackMedia>,
}

#[derive(Deserialize, Debug)]
struct TrackMedia {
    #[serde(rename = "Part")]
    parts: Vec<TrackPart>,
}

#[derive(Deserialize, Debug)]
struct TrackPart {
    key: String,
}

// --- Implementation ---

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Initialize Logging
    tracing_subscriber::fmt()
        .with_env_filter("info,plex_radio_rust=debug")
        .init();

    // 2. Load Config
    dotenvy::dotenv().ok();
    let plex_url = std::env::var("PLEX_URL")
        .expect("PLEX_URL must be set")
        .trim_end_matches('/')
        .to_string();
    let plex_token = std::env::var("PLEX_TOKEN").expect("PLEX_TOKEN must be set");
    let section_id_env = std::env::var("PLEX_SECTION_ID")
        .ok()
        .filter(|v| !v.is_empty());
    info!("Plex URL: {}", plex_url);
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    
    // Feature: Configurable Bitrate (default 320 kbps)
    let bitrate = std::env::var("PLEX_BITRATE")
        .unwrap_or_else(|_| "320".to_string())
        .parse()
        .expect("PLEX_BITRATE must be a number");
    // Feature: Configurable Audio Boost (default 100)
    let audio_boost = std::env::var("PLEX_AUDIO_BOOST")
        .unwrap_or_else(|_| "100".to_string())
        .parse()
        .expect("PLEX_AUDIO_BOOST must be a number");
    // Feature: Passthrough Mode (default false)
    let passthrough = std::env::var("PLEX_PASSTHROUGH").unwrap_or_else(|_| "false".to_string()) == "true";

    info!("Initializing Plex Radio...");

    // 3. Initialize HTTP Client
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    // 3.5. Resolve Section ID (Configured or Auto-detected)
    let section_id = match section_id_env {
        Some(id) => id,
        None => {
            info!("PLEX_SECTION_ID not set, attempting to auto-detect music library...");
            detect_music_section(&client, &plex_url, &plex_token).await?
        }
    };

    // 4. Pre-fetch Library Content (Cache Warming)
    info!("Fetching track list from Plex Library ID: {}", section_id);
    let tracks = fetch_library_tracks(&client, &plex_url, &plex_token, &section_id).await?;
    info!("Loaded {} tracks into rotation.", tracks.len());

    if tracks.is_empty() {
        error!("No tracks found. Please check your Section ID.");
        return Ok(());
    }

    let state = AppState {
        client,
        plex_url,
        plex_token,
        tracks: Arc::new(tracks),
        sessions: Arc::new(std::sync::Mutex::new(HashMap::new())),
        history: Arc::new(std::sync::Mutex::new(HashMap::new())),
        bitrate,
        audio_boost,
        passthrough,
    };

    // 5. Setup Router
    let app = Router::new()
        .route("/", get(web_interface))
        .route("/radio", get(stream_radio))
        .route("/now-playing", get(now_playing))
        .route("/search", get(search_tracks))
        .route("/health", get(|| async { "OK" }))
        .with_state(state);

    // 6. Start Server
    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse()?;
    info!("Radio server listening on http://{}", addr);
    
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// Fetches all music track keys from the specified library section.
/// Uses the Plex API endpoint: /library/sections/{id}/all?type=10 (Type 10 = Track)
async fn fetch_library_tracks(
    client: &Client,
    base_url: &str,
    token: &str,
    section_id: &str,
) -> anyhow::Result<Vec<Track>> {
    let url = format!("{}/library/sections/{}/all", base_url, section_id);
    
    let resp = client
        .get(&url)
        .header("X-Plex-Token", token)
        .header("Accept", "application/json")
        .query(&[("type", "10")]) // 10 is the Plex type ID for audio tracks
        .send()
        .await?
        .error_for_status()?
        .json::<PlexContainer>()
        .await?;

    let tracks: Vec<Track> = resp
        .media_container
        .metadata
        .into_iter()
        .map(|m| Track {
            key: m.rating_key,
            title: m.title,
            artist: m.artist,
            duration: m.duration,
        })
        .collect();

    Ok(tracks)
}

/// Detects the first available music library (type="artist") on the Plex server.
async fn detect_music_section(
    client: &Client,
    base_url: &str,
    token: &str,
) -> anyhow::Result<String> {
    let url = format!("{}/library/sections", base_url);

    let resp = client
        .get(&url)
        .header("X-Plex-Token", token)
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json::<PlexContainer>()
        .await?;

    let section = resp
        .media_container
        .directories
        .into_iter()
        .find(|d| d.section_type == "artist")
        .ok_or_else(|| anyhow::anyhow!("No music library (type='artist') found on this Plex server."))?;

    info!("Auto-detected Music Library: '{}' (ID: {})", section.title, section.key);
    Ok(section.key)
}

// --- Web Interface ---

async fn web_interface() -> Html<&'static str> {
    Html(r#"
<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Plex Radio</title>
    <style>
        :root {
            --primary: #e5a00d;
            --bg: #1e1e1e;
            --surface: #2d2d2d;
            --text: #e0e0e0;
        }
        body {
            background-color: var(--bg);
            color: var(--text);
            font-family: system-ui, -apple-system, sans-serif;
            display: flex;
            flex-direction: column;
            align-items: center;
            justify-content: center;
            height: 100vh;
            margin: 0;
            overflow: hidden;
        }
        .player-container {
            background: var(--surface);
            padding: 2rem;
            border-radius: 1rem;
            box-shadow: 0 10px 30px rgba(0,0,0,0.5);
            text-align: center;
            width: 90%;
            max-width: 400px;
            position: relative;
            z-index: 10;
        }
        .meta { margin-bottom: 1.5rem; }
        .meta h2 { margin: 0; font-size: 1.2rem; color: #fff; }
        .meta p { margin: 0.5rem 0 0; color: var(--primary); font-size: 1rem; font-weight: 500; }
        
        .progress-container {
            width: 100%;
            display: flex;
            align-items: center;
            gap: 10px;
            margin-bottom: 1.5rem;
            font-size: 0.8rem;
            color: #aaa;
        }
        .progress-bar {
            flex-grow: 1;
            height: 4px;
            background: #444;
            border-radius: 2px;
            overflow: hidden;
            cursor: pointer;
        }
        .progress-fill {
            height: 100%;
            background: var(--primary);
            width: 0%;
            transition: width 0.1s linear;
        }

        h1 { margin: 0 0 1.5rem 0; color: var(--primary); font-weight: 700; letter-spacing: -0.5px; }
        
        /* Visualizer Canvas */
        canvas {
            width: 100%;
            height: 100px;
            background: #000;
            border-radius: 0.5rem;
            margin-bottom: 1.5rem;
        }

        /* Custom Controls */
        .controls {
            display: flex;
            gap: 1rem;
            justify-content: center;
            margin-top: 1rem;
        }
        
        button {
            background: var(--primary);
            border: none;
            border-radius: 50%;
            width: 50px;
            height: 50px;
            cursor: pointer;
            color: #000;
            display: flex;
            align-items: center;
            justify-content: center;
            transition: transform 0.1s, filter 0.1s;
        }
        button:hover { filter: brightness(1.1); transform: scale(1.05); }
        button:active { transform: scale(0.95); }
        button svg { width: 24px; height: 24px; fill: currentColor; }

        audio { width: 100%; margin-top: 1rem; display: none; } /* Hidden, using custom controls */
        
        .status { font-size: 0.9rem; opacity: 0.7; margin-bottom: 1rem; min-height: 1.2em;}

        .volume-container {
            display: flex;
            align-items: center;
            gap: 10px;
            width: 100%;
            margin-top: 1.5rem;
            color: var(--primary);
        }
        .volume-container svg { width: 24px; height: 24px; fill: currentColor; }
        input[type=range] { flex-grow: 1; accent-color: var(--primary); cursor: pointer; }

        /* Search Modal */
        .search-modal {
            position: fixed;
            top: 0; left: 0; width: 100%; height: 100%;
            background: rgba(0,0,0,0.95);
            z-index: 100;
            display: none;
            flex-direction: column;
            padding: 2rem;
            box-sizing: border-box;
            backdrop-filter: blur(5px);
        }
        .search-modal.open { display: flex; }
        .search-header { display: flex; justify-content: flex-end; width: 100%; max-width: 600px; margin: 0 auto 1rem auto; }
        .close-btn { background: none; font-size: 2rem; color: #fff; padding: 0; width: auto; height: auto; cursor: pointer; }
        #searchInput {
            width: 100%; max-width: 600px; margin: 0 auto;
            padding: 1rem; font-size: 1.2rem;
            background: #333; border: 1px solid #555; color: #fff; border-radius: 0.5rem;
            outline: none;
        }
        #searchInput:focus { border-color: var(--primary); }
        #searchResults {
            flex-grow: 1; overflow-y: auto; width: 100%; max-width: 600px; margin: 1rem auto 0 auto;
        }
        .result-item {
            display: flex; justify-content: space-between; align-items: center;
            padding: 1rem; border-bottom: 1px solid #333; cursor: pointer;
            transition: background 0.2s;
            border-radius: 0.5rem;
        }
        .result-item:hover { background: #333; }
        .result-info { text-align: left; }
        .result-title { font-weight: bold; color: #fff; margin-bottom: 0.2rem; }
        .result-artist { color: var(--primary); font-size: 0.9rem; }
        .result-duration { font-size: 0.8rem; color: #888; }
        
        ::-webkit-scrollbar { width: 8px; }
        ::-webkit-scrollbar-track { background: #222; }
        ::-webkit-scrollbar-thumb { background: #555; border-radius: 4px; }
        ::-webkit-scrollbar-thumb:hover { background: var(--primary); }

        /* History */
        .history-container { width: 100%; margin-top: 2rem; text-align: left; border-top: 1px solid #333; padding-top: 1rem; }
        .history-title { color: #888; font-size: 0.8rem; margin-bottom: 0.5rem; text-transform: uppercase; letter-spacing: 1px; font-weight: bold; }
        .history-list { list-style: none; padding: 0; margin: 0; }
        .history-item { 
            display: flex; justify-content: space-between; align-items: center; 
            padding: 0.5rem 0; border-bottom: 1px solid #333; font-size: 0.9rem; color: #ccc; cursor: pointer; 
            transition: color 0.2s;
        }
        .history-item:hover { color: var(--primary); }
        .history-item:last-child { border-bottom: none; }
        .hist-title { font-weight: 500; }
        .hist-artist { font-size: 0.8rem; opacity: 0.6; }
    </style>
</head>
<body>
    <div class="player-container">
        <h1>Plex Radio</h1>
        <div class="meta">
            <h2 id="trackTitle">Waiting...</h2>
            <p id="trackArtist">...</p>
        </div>
        <div class="progress-container">
            <span id="currentTime">0:00</span>
            <div class="progress-bar"><div class="progress-fill" id="progressFill"></div></div>
            <span id="totalTime">0:00</span>
        </div>
        <canvas id="visualizer"></canvas>
        <div class="status" id="status">Ready to play</div>
        
        <div class="controls">
            <button id="playBtn" title="Play/Pause">
                <svg viewBox="0 0 24 24"><path d="M8 5v14l11-7z"/></svg>
            </button>
            <button id="stopBtn" title="Stop">
                <svg viewBox="0 0 24 24"><path d="M6 6h12v12H6z"/></svg>
            </button>
            <button id="skipBtn" title="Skip Track">
                <svg viewBox="0 0 24 24"><path d="M6 18l8.5-6L6 6v12zM16 6v12h2V6h-2z"/></svg>
            </button>
            <button id="shuffleBtn" title="Toggle Shuffle">
                <svg viewBox="0 0 24 24"><path d="M10.59 9.17L5.41 4 4 5.41l5.17 5.17 1.42-1.41zM14.5 4l2.04 2.04L4 18.59 5.41 20 17.96 7.46 20 9.5V4h-5.5zm.33 9.41l-1.41 1.41 3.13 3.13L14.5 20H20v-5.5l-2.04 2.04-3.13-3.13z"/></svg>
            </button>
            <button id="searchBtn" title="Search Library">
                <svg viewBox="0 0 24 24"><path d="M15.5 14h-.79l-.28-.27C15.41 12.59 16 11.11 16 9.5 16 5.91 13.09 3 9.5 3S3 5.91 3 9.5 5.91 16 9.5 16c1.61 0 3.09-.59 4.23-1.57l.27.28v.79l5 4.99L20.49 19l-4.99-5zm-6 0C7.01 14 5 11.99 5 9.5S7.01 5 9.5 5 14 7.01 14 9.5 11.99 14 9.5 14z"/></svg>
            </button>
        </div>

        <div class="volume-container">
            <button id="muteBtn" style="width: 30px; height: 30px; background: none; margin: 0; padding: 0;">
                <svg id="muteIcon" viewBox="0 0 24 24"><path d="M3 9v6h4l5 5V4L7 9H3zm13.5 3c0-1.77-1.02-3.29-2.5-4.03v8.05c1.48-.73 2.5-2.25 2.5-4.02zM14 3.23v2.06c2.89.86 5 3.54 5 6.71s-2.11 5.85-5 6.71v2.06c4.01-.91 7-4.49 7-8.77s-2.99-7.86-7-8.77z"/></svg>
            </button>
            <input type="range" id="volumeSlider" min="0" max="1" step="0.01" value="1">
        </div>

        <div class="history-container">
            <div class="history-title">Recently Played</div>
            <ul class="history-list" id="historyList"></ul>
        </div>
        
        <audio id="audio" crossorigin="anonymous" src="/radio"></audio>
    </div>

    <div class="search-modal" id="searchModal">
        <div class="search-header">
            <button class="close-btn" id="closeSearchBtn">&times;</button>
        </div>
        <input type="text" id="searchInput" placeholder="Search artist, title...">
        <div id="searchResults"></div>
    </div>

    <script>
        const audio = document.getElementById('audio');
        const playBtn = document.getElementById('playBtn');
        const stopBtn = document.getElementById('stopBtn');
        const skipBtn = document.getElementById('skipBtn');
        const shuffleBtn = document.getElementById('shuffleBtn');
        const muteBtn = document.getElementById('muteBtn');
        const muteIcon = document.getElementById('muteIcon');
        const status = document.getElementById('status');
        const canvas = document.getElementById('visualizer');
        const trackTitle = document.getElementById('trackTitle');
        const trackArtist = document.getElementById('trackArtist');
        const currentTime = document.getElementById('currentTime');
        const totalTime = document.getElementById('totalTime');
        const progressFill = document.getElementById('progressFill');
        const volumeSlider = document.getElementById('volumeSlider');
        const searchBtn = document.getElementById('searchBtn');
        const searchModal = document.getElementById('searchModal');
        const closeSearchBtn = document.getElementById('closeSearchBtn');
        const searchInput = document.getElementById('searchInput');
        const searchResults = document.getElementById('searchResults');
        const ctx = canvas.getContext('2d');
        const historyList = document.getElementById('historyList');

        // Icons
        const playIcon = '<svg viewBox="0 0 24 24"><path d="M8 5v14l11-7z"/></svg>';
        const pauseIcon = '<svg viewBox="0 0 24 24"><path d="M6 19h4V5H6v14zm8-14v14h4V5h-4z"/></svg>';
        const volOnIcon = '<path d="M3 9v6h4l5 5V4L7 9H3zm13.5 3c0-1.77-1.02-3.29-2.5-4.03v8.05c1.48-.73 2.5-2.25 2.5-4.02zM14 3.23v2.06c2.89.86 5 3.54 5 6.71s-2.11 5.85-5 6.71v2.06c4.01-.91 7-4.49 7-8.77s-2.99-7.86-7-8.77z"/>';
        const volOffIcon = '<path d="M16.5 12c0-1.77-1.02-3.29-2.5-4.03v2.21l2.45 2.45c.03-.2.05-.41.05-.63zm2.5 0c0 .94-.2 1.82-.54 2.64l1.51 1.51C20.63 14.91 21 13.5 21 12c0-4.28-2.99-7.86-7-8.77v2.06c2.89.86 5 3.54 5 6.71zM4.27 3L3 4.27 7.73 9H3v6h4l5 5v-6.73l4.25 4.25c-.67.52-1.42.93-2.25 1.18v2.06c1.38-.31 2.63-.95 3.69-1.81L19.73 21 21 19.73l-9-9L4.27 3zM12 4L9.91 6.09 12 8.18V4z"/>';

        // Audio Context for Visualizer
        let audioCtx, analyser, source;
        let isInit = false;
        
        let trackDuration = 0;
        let trackStartLocal = 0;
        let currentTrackKey = null;
        let isShuffle = true;

        // Client ID (Stable across sessions/skips)
        const clientId = localStorage.getItem('plex_radio_client_id') || Math.random().toString(36).substring(2, 15);
        localStorage.setItem('plex_radio_client_id', clientId);
        // Session ID for this client
        let sessionId = Math.random().toString(36).substring(2, 15);
        audio.src = `/radio?session=${sessionId}&client_id=${clientId}`;

        function initAudio() {
            if (isInit) return;
            isInit = true;
            
            const AudioContext = window.AudioContext || window.webkitAudioContext;
            audioCtx = new AudioContext();
            analyser = audioCtx.createAnalyser();
            analyser.fftSize = 256;
            
            source = audioCtx.createMediaElementSource(audio);
            source.connect(analyser);
            analyser.connect(audioCtx.destination);
            
            drawVisualizer();
        }

        function drawVisualizer() {
            requestAnimationFrame(drawVisualizer);
            
            const bufferLength = analyser.frequencyBinCount;
            const dataArray = new Uint8Array(bufferLength);
            analyser.getByteFrequencyData(dataArray);

            ctx.fillStyle = '#000';
            ctx.fillRect(0, 0, canvas.width, canvas.height);

            const barWidth = (canvas.width / bufferLength) * 2.5;
            let barHeight;
            let x = 0;

            for(let i = 0; i < bufferLength; i++) {
                barHeight = dataArray[i] / 2;
                ctx.fillStyle = `rgb(${barHeight + 100}, 160, 13)`; // Plex Orange-ish
                ctx.fillRect(x, canvas.height - barHeight, barWidth, barHeight);
                x += barWidth + 1;
            }
        }

        // Controls
        playBtn.addEventListener('click', () => {
            initAudio();
            if (audioCtx && audioCtx.state === 'suspended') audioCtx.resume();

            if (!audio.src || audio.src === window.location.href) {
                playStream();
                return;
            }

            if (audio.paused) {
                audio.play().catch(e => {
                    status.textContent = "Error: " + e.message;
                });
            } else {
                audio.pause();
            }
        });

        stopBtn.addEventListener('click', () => {
            audio.pause();
            audio.removeAttribute('src');
            status.textContent = "Stopped";
            playBtn.innerHTML = playIcon;
            trackTitle.textContent = "Stopped";
            trackArtist.textContent = "";
            trackDuration = 0;
            currentTrackKey = null;
            updateProgressUI(0, 0);
            
            ctx.fillStyle = '#000';
            ctx.fillRect(0, 0, canvas.width, canvas.height);
        });

        skipBtn.addEventListener('click', () => {
            status.textContent = "Skipping...";
            // Reload the source to trigger a new stream connection (new random song)
            playStream();
        });

        shuffleBtn.addEventListener('click', () => {
            isShuffle = !isShuffle;
            shuffleBtn.style.opacity = isShuffle ? '1' : '0.5';
            playStream();
        });

        document.querySelector('.progress-bar').addEventListener('click', (e) => {
            if (!trackDuration || !currentTrackKey) return;
            const rect = e.currentTarget.getBoundingClientRect();
            const x = e.clientX - rect.left;
            const pct = Math.max(0, Math.min(1, x / rect.width));
            const seekTime = Math.floor(pct * trackDuration);
            
            status.textContent = "Seeking...";
            playStream(`&track=${currentTrackKey}&offset=${seekTime}`);
        });

        volumeSlider.addEventListener('input', (e) => {
            audio.volume = e.target.value;
        });

        muteBtn.addEventListener('click', () => {
            audio.muted = !audio.muted;
            muteIcon.innerHTML = audio.muted ? volOffIcon : volOnIcon;
            muteBtn.style.opacity = audio.muted ? '0.5' : '1';
        });

        // Search Logic
        searchBtn.addEventListener('click', () => {
            searchModal.classList.add('open');
            searchInput.focus();
        });

        closeSearchBtn.addEventListener('click', () => {
            searchModal.classList.remove('open');
        });

        let searchTimeout;
        searchInput.addEventListener('input', (e) => {
            clearTimeout(searchTimeout);
            searchTimeout = setTimeout(() => {
                const q = e.target.value;
                if (q.length < 2) {
                    searchResults.innerHTML = '';
                    return;
                }
                fetch(`/search?q=${encodeURIComponent(q)}`)
                    .then(r => r.json())
                    .then(tracks => {
                        searchResults.innerHTML = tracks.map(t => `
                            <div class="result-item" onclick="playTrack('${t.key}')">
                                <div class="result-info">
                                    <div class="result-title">${t.title}</div>
                                    <div class="result-artist">${t.artist}</div>
                                </div>
                                <div class="result-duration">${formatTime(t.duration)}</div>
                            </div>
                        `).join('');
                    });
            }, 300);
        });

        window.playTrack = function(key) {
            searchModal.classList.remove('open');
            playStream(`&track=${key}`);
        };

        function playStream(params = '') {
            // Generate new session ID for every request to avoid race conditions
            sessionId = Math.random().toString(36).substring(2, 15);
            audio.src = `/radio?session=${sessionId}&client_id=${clientId}&shuffle=${isShuffle}${params}&t=${Date.now()}`;
            audio.play();
        }

        // Events
        audio.addEventListener('play', () => {
            playBtn.innerHTML = pauseIcon;
            status.textContent = "Streaming...";
        });
        
        audio.addEventListener('pause', () => {
            playBtn.innerHTML = playIcon;
            status.textContent = "Paused";
        });

        audio.addEventListener('error', (e) => {
            if (!audio.getAttribute('src')) return;
            status.textContent = "Stream Error. Retrying...";
            setTimeout(() => skipBtn.click(), 2000);
        });

        // Canvas sizing
        function resizeCanvas() {
            canvas.width = canvas.offsetWidth;
            canvas.height = canvas.offsetHeight;
        }
        window.addEventListener('resize', resizeCanvas);
        resizeCanvas();

        // Poll Metadata
        setInterval(() => {
            if (!audio.paused) {
                fetch(`/now-playing?session=${sessionId}&client_id=${clientId}`)
                    .then(r => {
                        if (r.ok) return r.json();
                        throw new Error('No track');
                    })
                    .then(data => {
                        trackTitle.textContent = data.title;
                        trackArtist.textContent = data.artist;
                        trackDuration = data.duration || 0;
                        currentTrackKey = data.key;
                        // Sync local time based on server elapsed
                        trackStartLocal = Date.now() - (data.elapsed || 0);
                        totalTime.textContent = formatTime(trackDuration);
                        
                        // Update History
                        if (data.history) {
                            historyList.innerHTML = data.history.map(t => `
                                <li class="history-item" onclick="playTrack('${t.key}')">
                                    <span class="hist-title">${t.title}</span>
                                    <span class="hist-artist">${t.artist}</span>
                                </li>
                            `).join('');
                        }
                    }).catch(() => {});
            }
        }, 2000);

        function updateProgressBar() {
            requestAnimationFrame(updateProgressBar);
            if (!trackDuration || audio.paused) return;
            const elapsed = Date.now() - trackStartLocal;
            updateProgressUI(elapsed, trackDuration);
        }
        updateProgressBar();

        function updateProgressUI(elapsed, duration) {
            const pct = duration > 0 ? Math.min(100, (elapsed / duration) * 100) : 0;
            progressFill.style.width = `${pct}%`;
            currentTime.textContent = formatTime(elapsed);
        }

        function formatTime(ms) {
            if (!ms || ms < 0) return "0:00";
            const s = Math.floor(ms / 1000);
            return `${Math.floor(s / 60)}:${(s % 60).toString().padStart(2, '0')}`;
        }
    </script>
</body>
</html>
    "#)
}

// --- Streaming Handler ---

struct SessionGuard {
    id: String,
    sessions: Arc<std::sync::Mutex<HashMap<String, (Track, SystemTime)>>>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = self.sessions.lock() {
            map.remove(&self.id);
        }
    }
}

/// Helper to build the Plex request (Passthrough or Transcode)
/// Separating this logic helps avoid compiler bugs with async-stream macros
async fn prepare_track_request(
    state: &AppState,
    track_key: &str,
    session_id: &str,
    offset_ms: u64,
) -> Option<reqwest::RequestBuilder> {
    if state.passthrough {
        // Passthrough: Fetch track metadata to get the actual file path
        let meta_url = format!("{}/library/metadata/{}", state.plex_url, track_key);
        let meta_resp = state.client.get(&meta_url)
            .header("X-Plex-Token", &state.plex_token)
            .header("Accept", "application/json")
            .send()
            .await;

        let part_key = match meta_resp {
            Ok(r) => match r.json::<TrackContainer>().await {
                Ok(c) => c.media_container.metadata.first()
                    .and_then(|m| m.media.first())
                    .and_then(|media| media.parts.first())
                    .map(|p| p.key.clone()),
                Err(_) => None,
            },
            Err(_) => None,
        };

        if let Some(pk) = part_key {
            let stream_url = format!("{}{}", state.plex_url, pk);
            Some(state.client.get(&stream_url)
                .header("X-Plex-Token", &state.plex_token))
        } else {
            error!("Failed to resolve file path for passthrough. Skipping.");
            None
        }
    } else {
        // Transcode: Use universal transcoder
        let base_url = state.plex_url.trim_end_matches('/');
        let transcode_url = format!("{}/music/:/transcode/universal/start.mp3", base_url);
        let path_param = format!("{}/library/metadata/{}?X-Plex-Token={}", base_url, track_key, state.plex_token);
        
        Some(state.client
            .get(&transcode_url)
            .header("X-Plex-Token", &state.plex_token)
            .header("X-Plex-Client-Identifier", "plex-radio-rust")
            .header("X-Plex-Product", "Plex Radio")
            .header("X-Plex-Version", "1.0")
            .header("X-Plex-Platform", "Generic")
            .header("X-Plex-Device", "Plex Radio")
            .header("X-Plex-Session-Id", session_id)
            .query(&[
                ("path", path_param),
                ("mediaIndex", "0".to_string()),
                ("partIndex", "0".to_string()),
                ("protocol", "http".to_string()),
                ("offset", (offset_ms / 1000).to_string()),
                ("fastSeek", "1".to_string()),
                ("directPlay", "0".to_string()),
                ("directStream", "1".to_string()),
                ("audioBoost", state.audio_boost.to_string()),
                ("maxAudioBitrate", state.bitrate.to_string()),
                ("context", "static".to_string()), 
                ("session", session_id.to_string()),
            ]))
    }
}

/// The main handler for the /radio endpoint.
/// Returns a continuous stream of MP3 data.
async fn stream_radio(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    // Create an async stream that yields Bytes
    let stream = async_stream::try_stream! {
        // Use provided session ID or generate one
        let session_id = params.get("session").cloned().unwrap_or_else(|| {
            format!("radio-{:x}", rand::thread_rng().gen::<u64>())
        });
        let client_id = params.get("client_id").cloned().unwrap_or_else(|| "anon".to_string());
        
        let mut initial_track_key = params.get("track").cloned();
        let mut initial_offset_ms = params.get("offset")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        
        let shuffle_mode = params.get("shuffle").map(|s| s != "false").unwrap_or(true);

        // RAII Guard to clean up session on disconnect
        let _guard = SessionGuard {
            id: session_id.clone(),
            sessions: state.sessions.clone(),
        };

        let mut current_track_index: Option<usize> = None;

        // Infinite loop: Pick a song, stream it, repeat.
        loop {
            // 1. Pick a random track
            let mut is_specific_request = false;
            let track = if let Some(key) = initial_track_key.take() {
                is_specific_request = true;
                if let Some(idx) = state.tracks.iter().position(|t| t.key == *key) {
                    current_track_index = Some(idx);
                    state.tracks[idx].clone()
                } else {
                    // Fallback if key not found
                    let mut rng = rand::thread_rng();
                    let idx = rng.gen_range(0..state.tracks.len());
                    current_track_index = Some(idx);
                    state.tracks[idx].clone()
                }
            } else {
                if shuffle_mode {
                    let mut rng = rand::thread_rng();
                    let idx = rng.gen_range(0..state.tracks.len());
                    current_track_index = Some(idx);
                    state.tracks[idx].clone()
                } else {
                    let next_idx = match current_track_index {
                        Some(i) => (i + 1) % state.tracks.len(),
                        None => rand::thread_rng().gen_range(0..state.tracks.len()),
                    };
                    current_track_index = Some(next_idx);
                    state.tracks[next_idx].clone()
                }
            };

            let track_key = track.key.clone();
            info!("Now Playing: {} - {}", track.artist, track.title);

            // 2. Determine Stream URL (Passthrough vs Transcode)
            let request_opt = prepare_track_request(&state, &track_key, &session_id, initial_offset_ms).await;
            
            let request = match request_opt {
                Some(req) => req,
                None => {
                    if is_specific_request { break; }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            
            // 3. Execute Request

            // Execute request
            let response = match request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    error!("Failed to fetch track from Plex: {}", e);
                    if is_specific_request { break; } // Don't fallback to random if specific track failed
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue; // Skip to next track on error
                }
            };

            if !response.status().is_success() {
                warn!("Plex returned non-success status: {}", response.status());
                if is_specific_request { break; } // Don't fallback to random if specific track failed
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }

            // Update session state (Metadata) only after successful connection
            if let Ok(mut map) = state.sessions.lock() {
                // If seeking, adjust start time so elapsed calculation is correct
                let start_time = SystemTime::now() - Duration::from_millis(initial_offset_ms);
                map.insert(session_id.clone(), (track.clone(), start_time));
            }
            
            // Update History (Add current track to history list)
            if let Ok(mut history_map) = state.history.lock() {
                let list = history_map.entry(client_id.clone()).or_default();
                list.insert(0, track.clone());
                if list.len() > 10 {
                    list.pop();
                }
            }

            // 4. Pipe the bytes to the listener
            let mut byte_stream = response.bytes_stream();
            let mut bytes_sent = 0;
            let stream_start = SystemTime::now();
            while let Some(chunk) = futures::StreamExt::next(&mut byte_stream).await {
                match chunk {
                    Ok(bytes) => { bytes_sent += bytes.len(); yield bytes; },
                    Err(e) => {
                        error!("Error reading bytes from Plex: {}", e);
                        break; // Break inner loop to pick new song (or disconnect)
                    }
                }
            }
            
            // Check for rapid failure (empty stream or very short duration)
            if bytes_sent < 1024 || stream_start.elapsed().unwrap_or(Duration::from_secs(0)) < Duration::from_secs(2) {
                warn!("Track finished too quickly ({} bytes). Possible transcoding error or empty file.", bytes_sent);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            
            // Reset offset for subsequent tracks in the playlist
            initial_offset_ms = 0;

            // Track finished, loop continues immediately to next track
        }
    };

    // Return the stream as the HTTP body with correct headers
    PlexStreamResponse(Box::pin(stream))
}

/// Returns the current track metadata for a given session.
async fn now_playing(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let session_id = params.get("session").map(|s| s.as_str()).unwrap_or("");
    let client_id = params.get("client_id").map(|s| s.as_str()).unwrap_or("anon");
    
    let sessions = state.sessions.lock().unwrap();
    match sessions.get(session_id) {
        Some((track, started_at)) => {
            let elapsed = started_at.elapsed().unwrap_or(Duration::from_secs(0)).as_millis() as u64;
            let history_map = state.history.lock().unwrap();
            let history = history_map.get(client_id).cloned().unwrap_or_default();
            // Skip the first element of history as it is the current track
            let previous_tracks: Vec<Track> = history.into_iter().skip(1).collect();
            
            let body = serde_json::json!({
                "title": track.title,
                "key": track.key,
                "artist": track.artist,
                "duration": track.duration,
                "elapsed": elapsed,
                "history": previous_tracks
            });
            Json(Some(body)).into_response()
        },
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Searches the cached track list for titles or artists matching the query.
async fn search_tracks(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let query = params.get("q").map(|s| s.to_lowercase()).unwrap_or_default();
    if query.len() < 2 {
        return Json(Vec::<Track>::new()).into_response();
    }

    let results: Vec<Track> = state.tracks.iter()
        .filter(|t| t.title.to_lowercase().contains(&query) || t.artist.to_lowercase().contains(&query))
        .take(50)
        .cloned()
        .collect();

    Json(results).into_response()
}

/// Implement IntoResponse for our stream to set headers manually
impl IntoResponse for PlexStreamResponse {
    fn into_response(self) -> Response {
        let body = Body::from_stream(self.0);
        
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "audio/mpeg")
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::CONNECTION, "keep-alive")
            .body(body)
            .unwrap()
    }
}

struct PlexStreamResponse(std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>);
