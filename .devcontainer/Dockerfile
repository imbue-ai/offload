FROM rust:1.92-bookworm

# Install system dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    curl \
    ca-certificates \
    pkg-config \
    libssl-dev \
    python3 \
    python3-pip \
    && rm -rf /var/lib/apt/lists/*

# Install cargo-nextest
RUN cargo install cargo-nextest --locked

# Ensure tool binaries are on PATH
ENV PATH="/root/.local/bin:/root/.cargo/bin:${PATH}"

# Install uv
RUN curl -LsSf https://astral.sh/uv/install.sh | sh

# Set working directory
WORKDIR /workspace

# Pre-compile Rust dependencies for faster builds
# Copy only dependency manifests first to leverage Docker layer caching
COPY Cargo.toml Cargo.lock ./

# Create dummy source files to satisfy cargo
RUN mkdir -p src && \
    echo "fn main() {}" > src/main.rs && \
    echo "" > src/lib.rs

# Build dependencies (release and debug)
RUN cargo build --release && cargo build

# Remove dummy source files (real source will be mounted/copied later)
RUN rm -rf src

CMD ["bash"]
