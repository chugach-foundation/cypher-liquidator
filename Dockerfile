FROM rust:1.60.0

RUN set -x \
 && apt update \
 && apt update \
 && apt install -y \
      buildkite-agent \
      clang \
      cmake \
      lcov \
      libudev-dev \
      mscgen \
      net-tools \
      rsync \
      sudo \
      unzip \
      \
 && apt remove -y libcurl4-openssl-dev \
 && rustup component add rustfmt \
 && rustup component add clippy \
 && rustc --version \
 && cargo --version

WORKDIR /usr/src/app

COPY . .

RUN cargo build --package cypher-liquidator --release

EXPOSE 8080

CMD [ "./target/release/cypher-liquidator", "-c", "./cfg./config.json" ]