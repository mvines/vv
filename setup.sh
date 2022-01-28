#!/usr/bin/env bash

set -ex
cd "$(dirname "$0")"
if [[ ! -d solana-v1.10 ]]; then
  git clone https://github.com/solana-labs/solana.git solana-v1.10
fi
