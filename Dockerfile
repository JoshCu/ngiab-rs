# Source of the model libraries, datasets and LSTM weights. Pinned so builds are
# reproducible; models.yml reads this line (sed -n 's/^ARG NGIAB_BASE=//p').
# Bump it together with a models-YYYY.MM.DD tag and a binary patch release.
ARG NGIAB_BASE=awiciroh/ngiab:v2.1.0

# Named stage: build args are not expanded inside `COPY --from=`, so every copy
# below goes through this stage instead.
FROM ${NGIAB_BASE} AS ngiab

# ---- build: bmi-driver, rs-route and ngiab-rs (statically linked netcdf/hdf5) ----
FROM ngiab AS build
# Defaults must match TOOL_VERSIONS (checked by a unit test); CI passes them as build args.
ARG BMI_DRIVER_VERSION=0.2.1
ARG RS_ROUTE_VERSION=0.3.1
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | bash -s -- -y --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}" CMAKE_POLICY_VERSION_MINIMUM=3.5
RUN dnf install -y gcc gcc-c++ make cmake && dnf clean all
WORKDIR /build
RUN cargo install bmi-driver --version "${BMI_DRIVER_VERSION}" --locked --features static --root /build/out
RUN cargo install rs_route --version "${RS_ROUTE_VERSION}" --locked --features static --root /build/out
COPY . /build/ngiab-rs
RUN cargo install --path /build/ngiab-rs --locked --features static --root /build/out

# ---- runtime (no Python: Python BMI models are unsupported) ----
FROM rockylinux:9.1 AS final
RUN dnf install -y libgfortran attr && dnf clean all
COPY --from=ngiab /dmod/datasets /dmod/datasets
COPY --from=ngiab /dmod/shared_libs /dmod/shared_libs
# Only the Rust LSTM weights; the rest of extern/lstm is the Python package.
COPY --from=ngiab /ngen/ngen/extern/lstm/trained_neuralhydrology_models /ngen/ngen/extern/lstm/trained_neuralhydrology_models
RUN echo "/dmod/shared_libs/" >> /etc/ld.so.conf.d/ngen.conf && ldconfig
COPY --from=build /build/out/bin/bmi-driver /build/out/bin/rs-route /build/out/bin/ngiab-rs /usr/local/bin/

# Data folder is mounted at /data (legacy NGIAB path kept as a symlink target)
RUN mkdir -p /ngen/ngen/data && ln -s /ngen/ngen/data /data
VOLUME /data
WORKDIR /data
ENV OMP_NUM_THREADS=1 TERM=xterm-256color
# docker run -it -v $PWD/my_run:/data joshcu/ngiab-rs                -> interactive TUI
# docker run    -v $PWD/my_run:/data joshcu/ngiab-rs /data --auto -j 8 -> headless
ENTRYPOINT ["/usr/local/bin/ngiab-rs"]
CMD ["/data"]
