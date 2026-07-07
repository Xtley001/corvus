# Node Setup — Corvus v1.0

> Base Mainnet IPC node: op-geth + op-node



## Prerequisites

- Ubuntu 22.04 LTS, 128 GB RAM, 4 TB NVMe SSD
- Go 1.21+, 10 Gbps network, L1 RPC URL (self-hosted Geth+Lighthouse recommended)

## 2.1 Build op-geth

```bash
wget https://go.dev/dl/go1.21.6.linux-amd64.tar.gz
sudo tar -C /usr/local -xzf go1.21.6.linux-amd64.tar.gz
echo 'export PATH=$PATH:/usr/local/go/bin' >> ~/.bashrc && source ~/.bashrc

git clone https://github.com/ethereum-optimism/op-geth.git && cd op-geth
git checkout $(git tag -l 'v*' | sort -V | tail -1)
make geth
sudo cp build/bin/geth /usr/local/bin/op-geth
cd ..
```

## 2.2 Configure op-geth (systemd service)

```bash
mkdir -p /data/base-geth
openssl rand -hex 32 > /data/base-geth/jwt.txt && chmod 600 /data/base-geth/jwt.txt

sudo tee /etc/systemd/system/op-geth.service > /dev/null <<'EOF'
[Unit]
Description=op-geth Base Mainnet
After=network.target

[Service]
Type=simple
User=ubuntu
ExecStart=/usr/local/bin/op-geth \
  --datadir=/data/base-geth \
  --networkid=8453 \
  --http --http.api=eth,net,web3,debug,txpool \
  --http.addr=127.0.0.1 --http.port=8545 \
  --ws --ws.api=eth,net,web3,debug,txpool \
  --ws.addr=127.0.0.1 --ws.port=8546 \
  --ipcpath=/tmp/base-geth.ipc \
  --authrpc.addr=127.0.0.1 --authrpc.port=8551 \
  --authrpc.jwtsecret=/data/base-geth/jwt.txt \
  --syncmode=snap \
  --gcmode=archive \
  --cache=49152 \
  --txpool.globalslots=10000 \
  --txpool.accountslots=128 \
  --metrics --metrics.addr=127.0.0.1 --metrics.port=6060
Restart=always
RestartSec=5
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable op-geth
sudo systemctl start op-geth
```

## 2.3 Build + configure op-node

```bash
git clone https://github.com/ethereum-optimism/optimism.git && cd optimism
git checkout $(git tag -l 'op-node/v*' | sort -V | tail -1)
make op-node
sudo cp op-node/bin/op-node /usr/local/bin/op-node
cd ..

sudo tee /etc/systemd/system/op-node.service > /dev/null <<'EOF'
[Unit]
Description=op-node Base Mainnet
After=op-geth.service

[Service]
Type=simple
User=ubuntu
# L1_RPC_URL: your Ethereum mainnet L1 RPC
ExecStart=/usr/local/bin/op-node \
  --network=base-mainnet \
  --l1=${L1_RPC_URL} \
  --l1.rpckind=basic \
  --l2=http://127.0.0.1:8551 \
  --l2.jwt-secret=/data/base-geth/jwt.txt \
  --rpc.addr=127.0.0.1 --rpc.port=9545 \
  --p2p.listen.tcp=9222 --p2p.listen.udp=9222
Restart=always
RestartSec=5
EOF

sudo systemctl enable op-node
sudo systemctl start op-node
```

## 2.4 Verify

```bash
# Fully synced = false
/usr/local/bin/op-geth attach /tmp/base-geth.ipc --exec "eth.syncing"

# Current block number
/usr/local/bin/op-geth attach /tmp/base-geth.ipc --exec "eth.blockNumber"

# IPC socket exists
ls -la /tmp/base-geth.ipc

# txpool accessible
/usr/local/bin/op-geth attach /tmp/base-geth.ipc --exec "txpool.status"
```

## Key flags explained

| Flag | Purpose |
|------|---------|
| `--ipcpath=/tmp/base-geth.ipc` | Unix socket for IPC — 0.1–1ms latency vs 20–100ms remote RPC |
| `--gcmode=archive` | Keep all historical state — required for position indexer bootstrap |
| `--cache=49152` | 48 GB state cache — fits Base state in RAM, eliminates disk I/O during sim |
| `--syncmode=snap` | Fast initial sync; `--gcmode=archive` retains all state after sync |
| `--txpool.globalslots=10000` | Larger mempool view for JIT Mode B opportunity detection |

## Sync time estimates

- Initial snap sync: 12–24 hours (Base archive state ~2 TB as of 2025)
- op-node catches up once op-geth is synced
- Monitor with: `journalctl -fu op-geth | grep "Syncing\|Imported"`
