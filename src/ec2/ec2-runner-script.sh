#!/bin/bash

export DEBIAN_FRONTEND=noninteractive

apt update && apt install -y build-essential unzip

# Without this we end up hitting the tiny default pids limit on larger hosts.
mkdir -p /etc/containers
touch /etc/containers/nodocker
cat > /etc/containers/containers.conf <<EOF
[containers]
pids_limit=1000000
default_ulimits=["nofile=100000:100000"]
EOF

ARCH="$(uname -m)"
curl "https://awscli.amazonaws.com/awscli-exe-linux-${ARCH}.zip" -o "/tmp/awscliv2.zip"
mkdir -p /var/tmp/aws-cli
cd /var/tmp/aws-cli
unzip /tmp/awscliv2.zip
sudo ./aws/install
cd -

sudo --login -u ubuntu bash -c 'curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'

ulimit -n $(ulimit -n -H)

if $INSTALL_RUNNER; then
    # Provision github actions runner
    # https://github.com/actions/runner/blob/main/docs/start/envlinux.md
    apt install -y liblttng-ust1t64 libkrb5-3 zlib1g libssl3 libicu78
    apt install -y docker.io docker-buildx jq python3-pip
    usermod -a -G docker ubuntu
    systemctl start docker
    sudo --login -u ubuntu bash -c 'mkdir actions-runner'
    sudo --login -u ubuntu bash -c 'cd actions-runner && curl -o runner.tar.gz -L https://github.com/actions/runner/releases/download/v2.335.1/actions-runner-linux-x64-2.335.1.tar.gz'
    sudo --login -u ubuntu bash -c 'cd actions-runner && tar xzf runner.tar.gz'
    sudo --login -u ubuntu bash -c 'cd actions-runner && ./run.sh --jitconfig "$JITCONFIG"'
    sleep 30
    shutdown now
else
    apt install -y podman-docker
    sudo --login -u ubuntu git config --global --add safe.directory /home/ubuntu/rust
    sudo --login -u ubuntu git init rust
    sudo --login -u ubuntu git -C rust remote add origin https://github.com/Mark-Simulacrum/rust
    sudo --login -u ubuntu git -C rust config --local gc.auto 0
    sudo --login -u ubuntu git -C rust fetch --no-tags --prune --no-recurse-submodules --depth=2 origin \
        +1c8426f372151fd686fd27497827d402d66855f8:refs/remotes/origin/automation/bors/auto
    sudo --login -u ubuntu git -C rust checkout --progress --force -B automation/bors/auto refs/remotes/origin/automation/bors/auto
    sudo --login -u ubuntu bash -c 'cd rust && ulimit -n $(ulimit -n -H) && ./src/ci/scripts/checkout-submodules.sh'
    sudo --login -u ubuntu bash -c 'cd rust && ulimit -n $(ulimit -n -H) && cargo run --manifest-path src/ci/citool/Cargo.toml run-local dist-x86_64-linux'
    sleep 30
    shutdown now
fi
