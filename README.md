# MTG FCI satellite image server over Iberia

A highly optimized Rust and Python hybrid web application to query, download, and compose true-color, ratio-sharpened images of the Iberian Peninsula from the latest EUMETSAT MTG (Meteosat Third Generation) FCI observations.

## Features
- **Fast Rust Backend**: An asynchronous `axum`-based HTTP server handles fast caching, cache-control, metadata lookups, and orchestrates the image processing worker safely.
- **Python Image Processor**: Utilizes `Satpy` and `Pyresample` to parse complex satellite NetCDF data, resample onto a precise geographical region over Iberia, and apply high-resolution ratio sharpening with the 0.5 km `vis_06_hr` channel.
- **Efficient Downloader**: Pre-filtered body chunk selection downloads only the exact segments covering Spain, Portugal, and the Balearic Islands, minimizing bandwidth usage and processing time.
- **Optimized & Self-contained Docker Image**:
  - Leverages a multi-stage Docker build separating Rust compilation, Python dependency resolution, and final runtime image creation.
  - Automatically fetches RSR (Relative Spectral Response) and Rayleigh correction files at build-time to function flawlessly offline or under restricted networks.
  - Pre-configured single-threaded scheduler (`DASK_SCHEDULER=synchronous`) to guarantee thread safety against NetCDF4 / HDF5 C libraries.

---

## Getting Started

### Prerequisites
- Docker or Podman
- EUMETSAT Data Store Credentials (API Key & Secret)

### Run with Docker

You can run the application by pulling the pre-built image directly from GitHub Container Registry (GHCR) or by building it locally.

#### Using Pre-built GHCR Image

```bash
# Pull the latest image
docker pull ghcr.io/tioruben/mtg-server:latest

# Run the server
docker run -d \
  -p 3000:3000 \
  --env-file .env \
  --name mtg-server \
  ghcr.io/tioruben/mtg-server:latest
```

#### Local Build

1. Create a `.env` file containing your EUMETSAT credentials:
   ```env
   EUMETSAT_CONSUMER_KEY=your_key_here
   EUMETSAT_CONSUMER_SECRET=your_secret_here
   ```

2. Build and run the image:
   ```bash
   # Build the optimized image
   docker build -t mtg:latest .

   # Run the server
   docker run -d -p 3000:3000 --env-file .env --name mtg-server mtg:latest
   ```

The application will be listening on `http://localhost:3000`.

---

## API Reference

### 1. Status Check
Returns a JSON snapshot of the server's state, indicating whether an image is available and the metadata of the satellite's observation time.

- **Endpoint**: `GET /api/status`
- **Response**:
  ```json
  {
    "state": "ready",
    "image_available": true,
    "image_url": "/image/latest.png",
    "generated_unix": 1784826123,
    "satellite_time": "2026-07-23T18:20:07Z",
    "message": "Latest observation ready"
  }
  ```

### 2. Request Latest Generation
Initiates a new generation worker if the cache TTL (10 minutes) has expired. This runs asynchronously and does not block the caller.

- **Endpoint**: `POST /api/latest`
- **Response**: Same as status endpoint.

### 3. Get Image
Serves the latest rendered high-resolution true-color PNG.

- **Endpoint**: `GET /image/latest.png`

---

## Configuration Options

You can configure the server using command line arguments or environment variables:

| Argument | Environment Variable | Default | Description |
|---|---|---|---|
| `--host` | `HTTP_HOST` | `0.0.0.0` | Host interface to bind to |
| `--port` | `HTTP_PORT` | `3000` | Port to expose the HTTP server on |
| `--cache-dir` | `MTG_CACHE_DIR` | `/app/cache` | Directory to save images and metadata |
| `--consumer-key` | `EUMETSAT_CONSUMER_KEY` | - | Your EUMETSAT API key |
| `--consumer-secret` | `EUMETSAT_CONSUMER_SECRET` | - | Your EUMETSAT API secret |

---

## Continuous Integration & Deployment

A fully configured GitHub Actions workflow (`.github/workflows/docker-publish.yml`) handles compiling, caching, and publishing images to **GitHub Container Registry (GHCR)** on pushes and release tags.

## License

This project is licensed under the [MIT License](LICENSE).
