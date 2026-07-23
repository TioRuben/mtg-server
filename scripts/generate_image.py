#!/usr/bin/env python3
"""Download the latest MTG FCI data over Iberia and compose a true-color PNG.

Steps:
  1. Get an access token from the EUMETSAT Data Store API.
  2. Find the latest FDHSI (EO:EUM:DAT:0662) and HRFI (EO:EUM:DAT:0665)
     products that share the same repeat cycle.
  3. Download only the body chunks 0031-0037 (the ones covering the Iberian
     Peninsula and the Balearic Islands) for both products.
  4. Compose Satpy's FCI ``true_color`` at 1 km, resample to a lat/lon grid
     over Iberia and ratio-sharpen it with the 0.5 km ``vis_06_hr`` channel.
  5. Write the PNG and a metadata JSON atomically.

Credentials are read from EUMETSAT_CONSUMER_KEY / EUMETSAT_CONSUMER_SECRET.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import re
import shutil
import sys
import tempfile
import time
import urllib.parse
import urllib.request
import warnings
from dataclasses import dataclass
from pathlib import Path

API_BASE = "https://api.eumetsat.int"
COLLECTION_FDHSI = "EO:EUM:DAT:0662"
COLLECTION_HRFI = "EO:EUM:DAT:0665"
# Body chunks that cover Iberia + Balearics (validated against real data).
CHUNKS = tuple(range(31, 38))
# West, South, East, North.
IBERIA_EXTENT = (-10.0, 35.0, 5.0, 44.5)
# ~0.005 deg =~ 0.5 km, matching the vis_06_hr native resolution.
RESOLUTION_DEG = 0.005

CHUNK_RE = re.compile(r"CHK-BODY.*_(\d{4})\.nc$")


def log(message: str) -> None:
    print(message, file=sys.stderr, flush=True)


def retrying(operation, *, attempts: int = 5, delay: float = 2.0):
    for attempt in range(attempts):
        try:
            return operation()
        except urllib.error.HTTPError as error:
            if error.code not in (429, 500, 502, 503, 504) or attempt == attempts - 1:
                raise
            wait = delay * (2**attempt)
            log(f"HTTP {error.code}, retrying in {wait:.0f}s")
            time.sleep(wait)
        except (urllib.error.URLError, TimeoutError) as error:
            if attempt == attempts - 1:
                raise
            wait = delay * (2**attempt)
            log(f"{error!r}, retrying in {wait:.0f}s")
            time.sleep(wait)


def http_json(url: str, *, headers: dict[str, str] | None = None, data: bytes | None = None) -> dict:
    def call() -> dict:
        request = urllib.request.Request(url, data=data, headers=headers or {})
        with urllib.request.urlopen(request, timeout=60) as response:
            return json.load(response)

    return retrying(call)


def get_token(key: str, secret: str) -> str:
    auth = base64.b64encode(f"{key}:{secret}".encode()).decode()
    payload = http_json(
        f"{API_BASE}/token",
        headers={
            "Authorization": f"Basic {auth}",
            "Content-Type": "application/x-www-form-urlencoded",
        },
        data=b"grant_type=client_credentials",
    )
    return payload["access_token"]


@dataclass
class Product:
    collection: str
    identifier: str
    start: str  # sensing start, e.g. "2026-07-23T16:30:07Z"
    entries: list[str]  # chunk entry file names to download


def search_latest(collection: str, count: int = 4) -> list[dict]:
    query = urllib.parse.urlencode(
        {"format": "json", "pi": collection, "sort": "start,time,0", "c": count}
    )
    payload = http_json(f"{API_BASE}/data/search-products/os?{query}")
    return payload.get("features", [])


def wanted_entries(feature: dict) -> list[str]:
    names = []
    for link in feature["properties"]["links"].get("sip-entries", []):
        title = link.get("title", "")
        match = CHUNK_RE.search(title)
        if match and int(match.group(1)) in CHUNKS:
            names.append(title)
    return sorted(names)


def pick_products() -> tuple[Product, Product]:
    """Pick the newest FDHSI+HRFI pair sharing the same sensing start."""
    fdhsi = search_latest(COLLECTION_FDHSI)
    hrfi = search_latest(COLLECTION_HRFI)

    def by_start(features: list[dict]) -> dict[str, dict]:
        return {f["properties"]["date"].split("/")[0]: f for f in features}

    fdhsi_by_start = by_start(fdhsi)
    hrfi_by_start = by_start(hrfi)
    common = sorted(set(fdhsi_by_start) & set(hrfi_by_start), reverse=True)
    if not common:
        raise RuntimeError("no repeat cycle available in both FDHSI and HRFI collections")

    start = common[0]

    def to_product(collection: str, feature: dict) -> Product:
        entries = wanted_entries(feature)
        if len(entries) != len(CHUNKS):
            raise RuntimeError(
                f"{collection}: expected {len(CHUNKS)} chunks, found {len(entries)}"
            )
        return Product(
            collection=collection,
            identifier=feature["properties"]["identifier"],
            start=start,
            entries=entries,
        )

    return (
        to_product(COLLECTION_FDHSI, fdhsi_by_start[start]),
        to_product(COLLECTION_HRFI, hrfi_by_start[start]),
    )


def download_product(product: Product, token: str, workdir: Path) -> list[Path]:
    base = (
        f"{API_BASE}/data/download/1.0.0/collections/"
        f"{urllib.parse.quote(product.collection, safe='')}/products/"
        f"{urllib.parse.quote(product.identifier, safe='')}/entry"
    )
    paths = []
    for name in product.entries:
        destination = workdir / name
        paths.append(destination)
        if destination.exists() and destination.stat().st_size > 0:
            continue
        url = f"{base}?name={urllib.parse.quote(name, safe='')}"
        log(f"downloading {name}")

        def fetch() -> None:
            request = urllib.request.Request(
                url, headers={"Authorization": f"Bearer {token}"}
            )
            with urllib.request.urlopen(request, timeout=600) as response:
                temporary = destination.with_suffix(".part")
                with open(temporary, "wb") as handle:
                    shutil.copyfileobj(response, handle, length=1 << 20)
                temporary.replace(destination)

        retrying(fetch)
    return paths


def compose(files: list[Path], output: Path) -> str:
    """Build a sharp, lightweight true-color PNG."""
    import numpy as np
    from pyresample import create_area_def
    from satpy import DataQuery, Scene
    from satpy.writers import get_enhanced_image

    warnings.filterwarnings("ignore")

    GRID_SIZE = 1500 

    area = create_area_def(
        "iberia_square",
        {"proj": "longlat", "datum": "WGS84"},
        area_extent=IBERIA_EXTENT, # (-10.0, 35.0, 5.0, 44.5)
        shape=(GRID_SIZE, GRID_SIZE * 1.2) 
    )

    vis_low = DataQuery(name="vis_06", resolution=1000)
    vis_high = DataQuery(name="vis_06", resolution=500)

    scene = Scene(filenames=[str(path) for path in files], reader="fci_l1c_nc")

    composite = "true_color"
    try:
        scene.load([composite, vis_low, vis_high], generate=False)
    except Exception as error:
        log(f"true_color failed ({error!r}); falling back to raw variant")
        composite = "true_color_raw_with_corrected_green"
        scene.load([composite, vis_low, vis_high], generate=False)

    # 2. Resample using 'nearest' (Blazing fast, low CPU usage)
    log("resampling to Iberia grid...")
    local = scene.resample(area, resampler="nearest")

    # 3. Let Satpy apply its native rayleigh & gamma enhancements FIRST
    log("generating enhanced true-color base image...")
    rgb_img = get_enhanced_image(local[composite])

    # Extract clean 0-1 numpy array [channels, height, width]
    rgb_data = rgb_img.data.compute().values  

    # 4. Extract high-res & low-res visible channels for ratio sharpening
    high = local[vis_high].compute().values
    low = local[vis_low].compute().values

    # 5. Apply safe ratio sharpening directly on the enhanced RGB array
    log("applying high-resolution sharpening...")
    with np.errstate(divide="ignore", invalid="ignore"):
        ratio = np.where((low > 0.05) & np.isfinite(high), high / low, 1.0)
    
    # Gently constrain ratio to prevent blowing out highlights
    ratio = np.clip(ratio, 0.6, 1.4)

    # Multiply RGB data by the ratio array across all color channels
    sharpened_data = np.clip(rgb_data * ratio[np.newaxis, :, :], 0.0, 1.0)

    # 6. Re-wrap into PIL/Trollimage and save cleanly
    log("rendering PNG...")
    from trollimage.xrimage import XRImage
    import xarray as xr

    final_xr = xr.DataArray(
        sharpened_data,
        dims=rgb_img.data.dims,
        coords=rgb_img.data.coords
    )
    
    final_image = XRImage(final_xr)
    temporary = output.with_suffix(".tmp.png")
    final_image.save(str(temporary))
    temporary.replace(output)

    start_time = scene.start_time
    return start_time.strftime("%Y-%m-%dT%H:%M:%SZ") if start_time else ""


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="Destination PNG")
    parser.add_argument("--metadata", type=Path, required=True, help="Destination JSON")
    parser.add_argument(
        "--workdir",
        type=Path,
        default=None,
        help="Directory for downloaded chunks (default: temp dir, deleted afterwards)",
    )
    args = parser.parse_args()

    key = os.environ.get("EUMETSAT_CONSUMER_KEY")
    secret = os.environ.get("EUMETSAT_CONSUMER_SECRET")
    if not key or not secret:
        log("EUMETSAT_CONSUMER_KEY / EUMETSAT_CONSUMER_SECRET are required")
        return 2

    log("requesting access token")
    token = get_token(key, secret)

    log("searching for the latest repeat cycle")
    fdhsi, hrfi = pick_products()
    log(f"selected repeat cycle starting {fdhsi.start}")

    keep_workdir = args.workdir is not None
    workdir = args.workdir or Path(tempfile.mkdtemp(prefix="mtg-chunks-"))
    workdir.mkdir(parents=True, exist_ok=True)

    try:
        files = download_product(fdhsi, token, workdir)
        files += download_product(hrfi, token, workdir)

        args.output.parent.mkdir(parents=True, exist_ok=True)
        satellite_time = compose(files, args.output) or fdhsi.start

        metadata = {
            "generated_unix": int(time.time()),
            "satellite_time": satellite_time,
            "product_id": fdhsi.identifier,
        }
        temporary = args.metadata.with_suffix(".tmp.json")
        temporary.write_text(json.dumps(metadata))
        temporary.replace(args.metadata)
        log("done")
        return 0
    finally:
        if not keep_workdir:
            shutil.rmtree(workdir, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
