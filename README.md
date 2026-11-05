# MockCam

Câmera virtual para Linux. Aparece como uma webcam comum (“MockCam”, `/dev/video10`) em Meet, Zoom, Teams, OBS e navegadores. Fontes que ela pode mandar ao ar:

- a câmera real ao vivo;
- um vídeo MP4 (ou mkv/mov/webm) em loop;
- um **loop gravado da própria câmera**;
- uma imagem;
- padrões de teste.

Aplica efeitos, usa GPU quando disponível e pode ser controlada pelo painel ou pela CLI.

## Arquitetura

```
mockcamd (daemon, Rust + GStreamer)
  Fonte (descartável) ──appsink──▶ FrameRouter ──▶ Efeitos (descartável, CPU ou GL) ──▶ slot "out"
                                       └─▶ slot "raw" ─▶ Gravador (loop da câmera, /dev/shm)
  Thread de saída (relógio próprio, fps fixo) ◀── slot "out" ──▶ appsrc ! v4l2sink /dev/video10

mockcam-panel (egui)  ── socket Unix (JSON por linha) ──▶ mockcamd
mockcam (CLI)         ──┘
```

- **A saída nunca para.** Fonte e efeitos rodam em pipelines separados. Trocar de fonte, virar o loop do vídeo, ligar/desligar a GPU ou trocar um filtro só reconstrói o pedaço afetado. Enquanto isso, a saída repete o último frame no fps configurado, e os apps consumidores não travam nem perdem a câmera.
- **Make-before-break:** a nova fonte só entra no ar depois de entregar o primeiro frame. Se falhar, a anterior continua.
- **Loop de vídeo sem emenda:** usa *segment seek*, sem EOS nem flush na virada.
- **GPU:**
  - O decode por hardware é escolhido pelo *rank* dos decoders: VA-API (Intel) ou NVDEC (NVIDIA).
  - **Auto** prefere a iGPU Intel, que não acorda a dGPU.
  - Os filtros GL rodam num bloco contínuo na GPU (um upload, um download).
  - Se algo falhar na GPU, o sistema cai sozinho para CPU e avisa no painel.
- **Preview do painel:** lê o próprio `/dev/video10`, então mostra exatamente o que os apps recebem. Nenhum frame trafega por IPC.
- **Câmera real:** é aberta só quando é a fonte (o LED apaga quando não está em uso).

## Instalação (Ubuntu 22.10+)

```bash
# 1. Dependências do sistema + módulo da câmera virtual (root, uma vez)
sudo packaging/mockcam-setup --deps

# 2. Rust (via rustup; o rustc do apt é antigo demais)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# 3. Compilar e instalar para o usuário (binários em ~/.local/bin + serviço systemd --user)
packaging/install.sh
```

O `mockcam-setup` grava `/etc/modprobe.d/mockcam.conf` com:

```
video_nr=10 card_label="MockCam" exclusive_caps=1
```

O `exclusive_caps=1` é necessário para o Chrome listar a câmera. O módulo carrega sozinho no boot.

## Uso

Abra o painel com `mockcam-panel`, ou use a CLI:

```bash
mockcam status
mockcam camera                 # câmera real ao vivo
mockcam file ~/video.mp4       # vídeo em loop (--once para tocar uma vez)
mockcam record 10 --loop       # grava 10 s do que está no ar e passa a repetir em loop
mockcam camera                 # volta ao vivo
mockcam gpu off                # desliga aceleração (auto|intel|nvidia|off)
mockcam mirror on
mockcam text "Ao vivo"
mockcam freeze on              # congela a imagem
mockcam output 1920 1080 30 yuy2
```

Para rodar sem instalar:

```bash
cargo run --release -p mockcamd
cargo run --release -p mockcam-gui
```

## Desenvolvimento

```bash
cargo test                      # testes de config, protocolo, slot, GPU
MOCKCAM_LOG=debug GST_DEBUG=3 cargo run -p mockcamd
```

Verificar fluidez:

```bash
gst-launch-1.0 v4l2src device=/dev/video10 ! videoconvert ! fpsdisplaysink
```

Rode isso e troque fontes, ligue e desligue a GPU, e deixe o vídeo virar o loop. O fps deve ficar estável. Para comparar CPU e GPU, use `top` e `intel_gpu_top` (ou `nvidia-smi dmon`), alternando `mockcam gpu off` e `mockcam gpu auto`.

## Escolhas e tradeoffs

| Decisão | Por quê | Custo |
|---|---|---|
| Rust + GStreamer | Decode HW, GL e `v4l2sink` prontos. Binário nativo, sem GC, threads seguras | Depende das libs GStreamer do sistema |
| v4l2loopback | É o que Zoom, Chrome, Firefox, Teams e OBS enumeram | Módulo de kernel (root uma vez) |
| egui | ~30–60 MB de RAM, binário único, sem WebView | Visual menos “nativo” que GTK |
| Daemon + painel | A câmera segue com o painel fechado; um crash da UI não derruba a chamada | Um processo a mais |
| Último frame (slot) em vez de fila | Latência mínima, a fonte nunca bloqueia e a saída nunca espera | Frames intermediários podem ser descartados se a fonte for mais rápida que a saída (desejado) |

## Limitações conhecidas

- O decode por GPU exige o `h264parse` (`gstreamer1.0-plugins-bad`).
- **Driver Intel `iHD` + gstreamer-vaapi 1.20:**
  - O `vaapidecodebin` e o `vaapipostproc` entregam frames **vazios (verde)** em memória de sistema. O MockCam desativa os dois e usa só os decoders (`vaapih264dec` etc.).
  - O `vaapijpegdec` lê o MJPEG da webcam (faixa completa) como faixa limitada, o que dobra a saturação. O `nvjpegdec` falha com MJPEG de webcams UVC (a câmera caía para ~10 fps). Os dois ficam desativados: o MJPEG vai para o `jpegdec` (CPU, barato em 720p) e os vídeos continuam na GPU.
  - O `vaapih264enc` não aceita `rate-control=cbr`.
  - Se a gravação pela GPU falhar, ela é refeita automaticamente pela CPU (`x264enc`).
- Medido aqui (1080p30 H.264 pesado → saída 720p30, imagem conferida):

  | Modo | CPU do daemon |
  |---|---|
  | Intel VA-API (Auto) | ~16% |
  | NVIDIA | ~17% |
  | GPU desligada | ~19% |

  A GPU corta o decode pela metade. O resto é escala e conversão na CPU, já que neste GStreamer o pós-processamento na GPU está quebrado (VA-API) ou não existe (CUDA).
- **Webcam e "FPS fixo":** muitas webcams UVC caem para ~10 fps em pouca luz (`exposure_dynamic_framerate`).
  - Com a opção ligada (padrão), o MockCam força 30 fps, e a imagem fica mais escura no escuro.
  - O valor original da webcam é restaurado quando a câmera é liberada.
  - Desligue no painel ou use `mockcam camera --dynamic-fps`.
- Com a GPU desligada, a escala e a conversão ficam na CPU. O `vaapipostproc` não entrega memória de sistema a partir de frames decodificados em software.
- Para compilar sem os pacotes `-dev`, só para desenvolvimento, use as variáveis `SYSTEM_DEPS_*_NO_PKG_CONFIG/LIB/SEARCH_NATIVE` apontando para as `.so.0` de runtime.

- Mudar a resolução ou o formato de saída exige que nenhum app esteja usando a câmera naquele momento.
- O loop "ping-pong" (vai e volta) ainda não foi implementado. O loop é sempre do início.
- O áudio dos vídeos é descartado (é uma câmera).
