# Nexo

[![License: AGPL-3.0-or-later](https://img.shields.io/badge/License-AGPL_3.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.88%2B-orange.svg)](https://www.rust-lang.org/)
[![UI: Slint](https://img.shields.io/badge/UI-Slint-blueviolet.svg)](https://slint.dev/)

**Nexo** é um aplicativo de comunicação ponto-a-ponto (P2P) nativo, local-first e de código aberto (AGPL-3.0), projetado para Windows e Linux. Cada instalação opera simultaneamente como cliente e nó de rede autônomo, permitindo comunicação de texto, voz, vídeo e compartilhamento de tela em rede local (LAN) ou pela internet sem dependência de servidores centrais obrigatórios ou runtimes pesados de navegador (sem Electron).

---

## 🌟 Princípios de Design

- **100% Nativo e Leve**: Construído em Rust e interface gráfica Slint compilada nativamente com aceleração por GPU/software, sem Electron ou WebViews.
- **Offline e Local-First por Design**: Descoberta e operação completas em LAN via mDNS, independente de conexão com a internet ou servidores DNS externos.
- **Verificação Criptográfica Ponta a Ponta**: Identidades persistentes Ed25519; todas as mensagens, convites de comunidade e sinais de chamada são assinados digitalmente.
- **Topologia Progressiva e Adaptativa**: Malha WebRTC P2P direta para pequenas chamadas (2 a 4 participantes) e eleição determinística de nó participante como SFU com migração *make-before-break* para grupos maiores.
- **Criptografia de Mídia Acima do Transporte**: Cifra de quadros em camadas para que nós roteadores SFU não consigam decodificar os fluxos de áudio e vídeo de outros participantes.

---

## 🏗️ Arquitetura do Workspace

O projeto é estruturado em uma arquitetura modular de crates Rust:

| Crate | Responsabilidade |
| :--- | :--- |
| [`crates/nexo-core`](crates/nexo-core) | Identidades Ed25519, convites assinados, credenciais de membros, mensagens, sinais de chamada, topologia/eleição SFU determinística e cifra de mídia E2E (`MediaFrameCipher`). |
| [`crates/nexo-store`](crates/nexo-store) | Persistência local em SQLite, paginação offline, recibos de entrega exatos, convergência de histórico e proteção contra replay. |
| [`crates/nexo-net`](crates/nexo-net) | Transporte de rede libp2p (TCP/QUIC), autenticação Noise, descoberta mDNS e protocolo de sinalização autenticado. |
| [`crates/nexo-video`](crates/nexo-video) | Captura de câmera (Media Foundation no Windows / V4L2 no Linux), captura de tela (Windows Graphics Capture / XDG Portal + PipeWire) e sondagem de aceleração por hardware (GPU/VA-API/AMF/MFT). |
| [`crates/nexo-media`](crates/nexo-media) | Sessões WebRTC (DTLS/SRTP), codec de áudio Opus puro em Rust, codec de vídeo VP8 autocontido, buffer de jitter com FEC e gerenciamento de dispositivos CPAL. |
| [`crates/nexo-app`](crates/nexo-app) | Interface desktop nativa em Slint, orquestração de chamadas, catálogo dinâmico de dispositivos de áudio/vídeo e persistência de estado. |

---

## 🚀 Funcionalidades Implementadas

- [x] **Identidade e Segurança**: Chaves Ed25519 salvas localmente, mensagens e sinais de presença assinados e verificados.
- [x] **Comunidades e Mensagens Offline**: Criação e entrada por convites assinados com expiração, sincronização automática ao reconectar.
- [x] **Descoberta LAN**: Descoberta automática de pares na rede local via mDNS.
- [x] **Voz WebRTC P2P**: Áudio Opus (20 ms, VBR, FEC, DTX), troca de microfone/alto-falante a quente sem queda de chamada, buffer de jitter com ocultação de perda de pacotes.
- [x] **Vídeo WebRTC P2P**: Captura de câmera e tela, conversão para I420, codificador/decodificador de vídeo VP8 e empacotamento RTP com adaptação de taxa de bits (RTCP REMB).
- [x] **Topologia SFU & E2E Crypto**: Eleição determinística de nó hospedeiro por capacidade de rede/hardware, failover automático de standby por heartbeat e cifra autenticada SHA-256 para pacotes de mídia.
- [x] **Interface Slint Desktop**: Navegação em comunidades, canais de texto, painel de controle de chamada, lista de participantes conectados e seletores de microfone, saída de áudio e câmera.

---

## 🛠️ Compilação e Testes

### Pré-requisitos
- **Rust** 1.88 ou superior (toolchain estável ou GNU).
- No Windows: Toolchain `x86_64-pc-windows-gnu` ou `x86_64-pc-windows-msvc`.
- No Linux (Ubuntu/Debian):
  ```bash
  sudo apt update && sudo apt install -y build-essential pkg-config libv4l-dev libasound2-dev
  ```

### Executando Testes
Para rodar toda a suíte de testes do workspace:
```bash
cargo test --workspace
```

### Verificação Estrita de Formatação e Lints
```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

### Executando o Aplicativo
```bash
cargo run -p nexo-app
```

### Exemplos e Probes de Mídia
```bash
# Teste de saída de áudio WASAPI/ALSA:
cargo run -p nexo-media --example output_silence

# Sondagem de câmeras, GPU e aceleração de codecs:
cargo run -p nexo-video --example capabilities

# Preview de captura de câmera:
cargo run -p nexo-video --example capture_preview

# Captura de tela:
cargo run -p nexo-video --example capture_screen
```

---

## 📖 Documentação

- [Arquitetura Geral](docs/architecture.md)
- [Protocolo e Sinalização](docs/protocol.md)
- [Modelo de Ameaças e Segurança](docs/threat-model.md)
- [Roadmap de Desenvolvimento](docs/roadmap.md)
- [Registro de Continuação e Checkpoints](docs/continuation.md)

---

## 📄 Licença

Este projeto é licenciado sob a **AGPL-3.0-or-later**. Consulte o arquivo [LICENSE](LICENSE) para mais informações.
