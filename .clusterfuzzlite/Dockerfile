# The image ClusterFuzzLite builds the fuzz targets in (fuzz/). Pinned by
# digest; dependabot keeps it current.
FROM gcr.io/oss-fuzz-base/base-builder-rust@sha256:3428d0a90b1e7efbe2ba947bd1d0c9a70594986b4b6553804547f1fb24cfc92f

# The image's own nightly predates the crate's rust-version.
ENV RUSTUP_TOOLCHAIN=nightly-2026-10-11
RUN rustup toolchain install nightly-2026-10-11 --profile minimal --component rust-src

COPY . $SRC/chanto
WORKDIR $SRC/chanto
COPY .clusterfuzzlite/build.sh $SRC/
