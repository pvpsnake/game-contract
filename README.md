# 🐍 Snake Game - Solana Program

A onchain snake game built on Solana where players can compete in PvP matches with SOL

## 🎮 Game Mechanics

### How It Works
1. **Create Lobby**: Player creates a game lobby with a SOL stake
2. **Join Game**: Another player joins with matching stake
3. **Play Snake**: Players compete in the classic snake game
4. **Winner Takes All**: Winner receives both stakes minus 5% commission

## 🚀 Quick Start

### Prerequisites
- [Rust](https://rustup.rs/)
- [Solana CLI](https://docs.solana.com/cli/install-solana-cli-tools)
- [Anchor CLI](https://www.anchor-lang.com/docs/installation)
- [Node.js](https://nodejs.org/)

### Installation

```bash
# Clone the repository
git clone https://github.com/pvpsnake/snake-game-solana.git
cd snake-game-solana

# Install dependencies
npm install

# Build the contract
anchor build


### Deployment

```bash

anchor deploy --provider.cluster devnet
```

## 🌐 Links

- **Website**: [https://pvpsnake.io](https://pvpsnake.io)
- **X (Twitter)**: [@PvPSnakeSOL](https://x.com/PvPSnakeSOL)

## ⚠️ Disclaimer

This software is provided "as is" without warranty. Users participate at their own risk. Please read our [security policy](security.txt) and understand the risks before using.

---

**Built with ❤️ on Solana**