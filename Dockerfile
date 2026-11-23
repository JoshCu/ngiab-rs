# ---- build: bmi-driver, rs-route and ngiab-rs (statically linked netcdf/hdf5) ----
FROM awiciroh/ngiab AS build
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | bash -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"
RUN dnf install -y g++ cmake gfortran python3.11-devel
WORKDIR /build
RUN cargo install bmi-driver --features static --root /build/out
RUN cargo install rs_route --features static --root /build/out
COPY . /build/ngiab-rs
RUN cargo install --path /build/ngiab-rs --features static --root /build/out

# ---- runtime ----
FROM rockylinux:9.1 AS final
RUN dnf install -y python3.11 libgfortran attr && dnf clean all
COPY --from=awiciroh/ngiab /dmod/datasets /dmod/datasets
COPY --from=awiciroh/ngiab /dmod/shared_libs /dmod/shared_libs
COPY --from=awiciroh/ngiab /ngen/ngen/extern/lstm/ /ngen/ngen/extern/lstm
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
