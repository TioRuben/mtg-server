# syntax=docker/dockerfile:1

# --- Rust Builder ---
FROM rust:1-slim-bookworm AS rust-builder
WORKDIR /app

# Install standard build dependencies if needed
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Cache dependencies build step
COPY Cargo.toml ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release

# Copy the real source code and assets (assets/index.html is embedded at compile-time)
COPY src/ ./src/
COPY assets/ ./assets/

# Touch main.rs to ensure cargo rebuilds with the actual code
RUN touch src/main.rs
RUN cargo build --release

# --- Python Builder ---
FROM python:3.11-slim-bookworm AS python-builder
WORKDIR /app

# Install compilation tools for python wheels if needed
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    python3-dev \
    && rm -rf /var/lib/apt/lists/*

# Setup virtual environment
RUN python -m venv /app/venv
ENV PATH="/app/venv/bin:$PATH"

# Install Python requirements
COPY requirements.txt ./
RUN pip install --no-cache-dir --upgrade pip && \
    pip install --no-cache-dir -r requirements.txt

# --- Final Runner ---
FROM python:3.11-slim-bookworm AS runner
WORKDIR /app

# Install runtime system dependencies (libgomp1 is commonly required by numpy/scipy/netCDF4)
RUN apt-get update && apt-get install -y --no-install-recommends \
    libgomp1 \
    && rm -rf /var/lib/apt/lists/*

# Copy python virtual environment and scripts
COPY --from=python-builder /app/venv /app/venv
COPY scripts/ /app/scripts/
COPY --chown=app:app vendor/pyspectral/ /opt/pyspectral-fallback/

# Copy Rust compiled binary
COPY --from=rust-builder /app/target/release/mtg /usr/local/bin/mtg

# Setup non-root user for security
RUN useradd -u 10001 -m -U app && \
    mkdir -p /var/lib/mtg/cache && \
    chown -R app:app /var/lib/mtg

# Configure execution environment
ENV PYTHON=/app/venv/bin/python
ENV PATH="/app/venv/bin:$PATH"
ENV HTTP_HOST=0.0.0.0
ENV HTTP_PORT=3000
ENV MTG_CACHE_DIR=/var/lib/mtg/cache
ENV MTG_PROCESSOR=/app/scripts/generate_image.py
ENV DASK_SCHEDULER=synchronous

# Persist downloaded/generated images across container recreation
VOLUME ["/var/lib/mtg/cache"]

USER app

# Pre-download Relative Spectral Response (RSR) and Rayleigh correction lookup tables (LUTs)
RUN for attempt in 1 2; do \
        if download_rsr.py && download_atm_correction_luts.py -a rayleigh_only; then \
            exit 0; \
        else \
            status=$?; \
        fi; \
        echo "pyspectral data download failed (attempt $attempt/2; exit $status)" >&2; \
        rm -rf /home/app/.local/share/pyspectral; \
        if [ "$attempt" -eq 2 ]; then \
            break; \
        fi; \
        sleep $((attempt * 10)); \
    done; \
    echo "using vendored Pyspectral FCI data" >&2; \
    mkdir -p /home/app/.local/share/pyspectral; \
    cp -a /opt/pyspectral-fallback/. /home/app/.local/share/pyspectral/

EXPOSE 3000

ENTRYPOINT ["mtg"]
