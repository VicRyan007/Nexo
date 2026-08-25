# Manual do Usuário: Nexo Desktop

Bem-vindo ao **Nexo**, a plataforma nativa de colaboração peer-to-peer (P2P), segura, local-first e sem servidores centrais para comunicação de texto, voz, vídeo e transferência de arquivos.

---

## 📑 Índice
1. [Visão Geral & Filosofia](#visão-geral--filosofia)
2. [Instalação & Execução](#instalação--execução)
   - [Windows](#windows)
   - [Linux](#linux)
3. [Primeiros Passos](#primeiros-passos)
   - [Criando sua Identidade Criptográfica](#criando-sua-identidade-criptográfica)
   - [Criando uma Nova Comunidade](#criando-uma-nova-comunidade)
   - [Entrando em uma Comunidade via Convite](#entrando-em-uma-comunidade-via-convite)
4. [Chat e Recursos de Mensagens](#chat-e-recursos-de-mensagens)
   - [Formatação Markdown e Emojis](#formatação-markdown-e-emojis)
   - [Transferência P2P de Arquivos](#transferência-p2p-de-arquivos)
   - [Notas de Voz Rápidas](#notas-de-voz-rápidas)
5. [Chamadas de Voz e Vídeo WebRTC](#chamadas-de-voz-e-vídeo-webrtc)
   - [Entrando na Voz](#entrando-na-voz)
   - [Ligar/Desligar Câmera e Compartilhar Tela](#ligardesligar-câmera-e-compartilhar-tela)
   - [Configuração e Troca a Quente de Dispositivos](#configuração-e-troca-a-quente-de-dispositivos)
6. [Segurança e Criptografia](#segurança-e-criptografia)
7. [Solução de Problemas (Troubleshooting)](#solução-de-problemas-troubleshooting)

---

## 🌟 Visão Geral & Filosofia

O Nexo foi projetado sob os seguintes princípios:
- **Zero Nuvem / Sem Servidor Central**: Nenhuma empresa armazena suas mensagens, senhas ou chamadas.
- **Local-First**: Suas conversas e dados pertencem ao seu computador (banco SQLite local) e continuam disponíveis sem acesso à internet.
- **Criptografia Ponta a Ponta Real**: Assinaturas digitais Ed25519 em cada mensagem e criptografia autenticada em fluxos de mídia.
- **Descoberta LAN Automática**: Nós na mesma rede local se encontram automaticamente via mDNS.

---

## 💻 Instalação & Execução

### Windows
1. Baixe o arquivo `nexo-<versão>-windows-x86_64.zip` na página de [Releases](https://github.com/VicRyan007/Nexo/releases).
2. Extraia o conteúdo em uma pasta de sua preferência.
3. Dê um duplo clique no executável `nexo.exe`.

### Linux
- **Debian / Ubuntu (.deb)**:
  ```bash
  sudo dpkg -i nexo_1.0.0_amd64.deb
  nexo
  ```
- **Tarball Portátil (.tar.gz)**:
  ```bash
  tar -xzf nexo-1.0.0-linux-x86_64.tar.gz
  ./nexo
  ```

---

## 🚀 Primeiros Passos

### Criando sua Identidade Criptográfica
Ao abrir o Nexo pela primeira vez, uma chave criptográfica **Ed25519** de 256 bits é gerada automaticamente e armazenada de forma segura no seu perfil de usuário local (`~/.nexo/identity.key` ou `%LOCALAPPDATA%\Nexo\identity.key`).

### Criando uma Nova Comunidade
1. Na barra lateral esquerda, digite um nome no campo **Criar Comunidade** (ex: `Devs`).
2. Clique em **Criar e gerar convite**.
3. O Nexo criará a comunidade com o canal padrão `# geral` e exibirá um código de convite assinado no formato `NEXO1...`.
4. Copie esse código e envie para seus amigos.

### Entrando em uma Comunidade via Convite
1. Recebeu um convite de um colega? Cole o código no campo **Entrar com convite**.
2. Clique em **Entrar**.
3. Seu nó se conectará automaticamente com o criador e com os outros membros na rede local, sincronizando o histórico de mensagens instantaneamente.

---

## 💬 Chat e Recursos de Mensagens

### Formatação Markdown e Emojis
O chat do Nexo suporta formatação em tempo real:
- `**negrito**` -> **negrito**
- `*itálico*` -> *itálico*
- `` `código inline` `` -> `código inline`
- ` ```linguagem ... ``` ` -> bloco de código destacado
- Shortcodes de emoji automáticos: `:rocket:`, `:smile:`, `:+1:`, `:fire:`, `:lock:`, `:mic:`, `:camera:`.

### Transferência P2P de Arquivos
1. Clique no botão **`+`** ao lado da barra de mensagens.
2. Escolha um arquivo no seletor nativo. O limite atual é 256 MB.
3. O arquivo será assinado, fragmentado em pedaços de 64 KB com hashes SHA-256 e transmitido diretamente aos membros autorizados conectados via protocolo `/nexo/file-transfer/0.1.0`.
4. Downloads aceitos são gravados automaticamente na pasta `downloads` do perfil local do Nexo após a verificação do hash final.

### Notas de Voz Rápidas
1. Clique no botão **`Voz`** ao lado do campo de mensagem.
2. O Nexo captura o microfone selecionado em PCM mono de 48 kHz por até 60 segundos.
3. Clique novamente para concluir. A nota é salva como WAV e enviada aos membros autorizados pelo mesmo transporte P2P de arquivos.

---

## 🎙️ Chamadas de Voz e Vídeo WebRTC

### Entrando na Voz
1. Selecione a comunidade desejada.
2. No painel de voz, clique em **Entrar na voz**.
3. O status mudará para **CONECTADO** em verde esmeralda.
4. O áudio utiliza o codec **Opus** em 48 kHz. A captura aplica supressão de ruído e AEC/NLMS quando existe referência de playback; a implementação atual usa o último frame enviado ao dispositivo de saída.

### Ligar/Desligar Câmera e Compartilhar Tela
- **Câmera**: Clique em **Cam ON / Cam OFF** para transmitir sua webcam em tempo real (VP8 por padrão; H.264 acelerado fica disponível quando a conexão negocia esse codec).
- **Compartilhar Tela**: Clique em **Comp. Tela / Parar Tela** para transmitir sua área de trabalho com baixa latência.
- O vídeo do participante remoto e sua prévia local aparecem automaticamente no topo do painel principal.

### Configuração e Troca a Quente de Dispositivos
Nos seletores **MICROFONE**, **ALTO-FALANTE** e **CAMERA**, você pode alternar livremente seus periféricos durante a chamada sem interrupção. O Nexo memoriza suas preferências para as próximas sessões.

---

## 🛡️ Segurança e Criptografia
- **Mensagens de Comunidade**: Assinadas por Ed25519 e cifradas no envelope com ChaCha20-Poly1305 usando o segredo/epoch da comunidade. Convites antigos continuam compatíveis, mas não têm o segredo privado de grupo e usam o modo legado.
- **Mensagens Diretas 1-a-1**: Em uma comunidade, use a seção `MENSAGENS DIRETAS` na barra lateral, selecione um membro autorizado e envie pelo compositor. A entrega ao vivo e a sincronização após reconexão preservam o envelope cifrado e o histórico local.
- **Grupos inspirados em MLS**: commits assinados de entrada e remoção, epochs e histórico de segredos são persistidos e sincronizados entre os membros autorizados. O fundador pode remover um membro na seção `MEMBROS`; uma remoção gera envelopes X25519 individuais para os membros restantes e troca a chave da chamada. A interoperabilidade RFC 9420 e key packages padrão ainda não fazem parte do protocolo.
- **Fluxos de Mídia**: O transporte WebRTC usa DTLS/SRTP e `MediaFrameCipher` adiciona autenticação e cifragem por frame acima do enlace. O relay encaminha o envelope sem decifrá-lo; cada publicador recebe uma trilha negociada própria e aparece como um quadro separado na galeria, limitada a oito quadros para preservar a resposta dos controles.

---

## ❓ Solução de Problemas (Troubleshooting)

### Não encontro meus amigos na rede local
1. Certifique-se de que ambos os computadores estão conectados na mesma sub-rede Wi-Fi/Ethernet.
2. Verifique se o Firewall do Windows ou `iptables/ufw` no Linux permite tráfego multicast UDP (porta 5353 para mDNS) e portas UDP efêmeras para WebRTC.
3. Confirme se ambos utilizaram convites válidos gerados pela mesma comunidade.
4. A descoberta repete tentativas automaticamente por alguns segundos; se a rede bloqueia multicast, cole o endereço completo do convite ou reinicie a conexão depois de liberar o firewall.

### Áudio com eco ou ruído de fundo
1. O pipeline de DSP do Nexo inclui cancelador de eco acústico adaptativo (AEC) e filtro de ruído RMS.
2. Recomendamos o uso de fones de ouvido para máxima clareza sonora.

### O aplicativo fecha ao entrar em voz ou vídeo
1. Abra novamente o Nexo e repita a ação para confirmar o problema.
2. Consulte `crash.log` dentro da pasta local de dados do Nexo e guarde o trecho iniciado por `===`.
3. Verifique se o microfone, alto-falante, câmera e monitor continuam disponíveis; o Nexo tenta se recuperar de desconexões, mas o registro ajuda a identificar falhas do driver.
4. Se a chamada permanecer em negociação, observe o status da chamada: mensagens com `ICE/DTLS falhou` indicam NAT/firewall ou servidores STUN/TURN, enquanto uma falha de dispositivo aparece como erro de áudio/vídeo.

### Chamada entre redes diferentes (opcional)
O Nexo continua funcionando sem internet e sem servidor externo na LAN. Para habilitar candidatos
STUN, defina `NEXO_STUN_SERVERS` com URLs separadas por vírgula, por exemplo
`stun:stun.example.org:3478`. Para TURN, use entradas separadas por ponto e vírgula no formato
`turn:relay.example.org:3478|usuario|senha`. Entradas incompletas são ignoradas e não impedem o
aplicativo de iniciar. Para descoberta opcional de peers fora da LAN, defina `NEXO_KAD_BOOTSTRAP`
com endereços completos e autenticados terminados em `/p2p/<PeerId>`, separados por ponto e
vírgula. Para atravessar NAT também defina `NEXO_RELAY_SERVERS` com endereços autenticados de
Circuit Relay v2, no mesmo formato e separados por ponto e vírgula. O Nexo reserva o relay,
anuncia um endereço `/p2p-circuit` e tenta migrar para uma conexão direta com DCUtR. Sem
`NEXO_RELAY_SERVERS`, o modo local e as conexões diretas permanecem inalterados.

Uma instalação pode hospedar um relay v2 para os demais nós. Inicie-a com
`NEXO_RELAY_SERVER=1` e, opcionalmente, `NEXO_RELAY_LISTEN_PORT=4001`; encaminhe a porta TCP e
UDP no roteador. Para uso fora da LAN, defina também
`NEXO_RELAY_PUBLIC_ADDRESS=/ip4/endereco-publico/tcp/4001` e use o endereço autenticado exibido
pela instalação como entrada de `NEXO_RELAY_SERVERS`. O recurso é desligado por padrão e possui
limites internos para não transformar uma máquina comum em relay ilimitado.

Interfaces Tailscale e ZeroTier também podem aparecer nos convites; interfaces virtuais de
containers e adaptadores de teste continuam sendo filtradas para evitar endereços inutilizáveis.
Para testar exclusivamente o caminho por convite ou relay, defina `NEXO_DISABLE_MDNS=1` antes de
abrir o Nexo; isso desliga apenas a descoberta automática mDNS e não desativa conexões diretas,
Circuit Relay ou o restante da rede.
