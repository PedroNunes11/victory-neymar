# GTA VI (Xbox Series) no PC — versão do Fdx

Esta branch é a versão do projeto que melhor rodou nos nossos testes. É um fork do
[VIctory](https://github.com/Marvisak/VIctory), uma camada de compatibilidade que roda um
jogo de Xbox Series no Windows a partir dos arquivos **da sua própria cópia**.

**Este repositório não contém nada do jogo** (nem arquivos, nem código, nem imagens). Você
precisa da sua própria cópia do jogo, versão Xbox Series **1.0.2.1**, já extraída em disco.
Este projeto não tem relação com a Rockstar Games, a Take-Two ou a Microsoft.

## Como funciona, em poucas palavras

- O Xbox Series usa o mesmo tipo de processador do PC (x86-64): o código do jogo roda direto
  no processador, sem emulação. As bibliotecas do console (memória, arquivos, gráficos, áudio)
  são reimplementadas para Windows.
- Os shaders do jogo são código para a GPU AMD do console. Um tradutor converte cada um (mais
  de 26 mil) para DirectX 12 e guarda num cache.
- A parte difícil é a memória: no console, CPU e placa de vídeo dividem a mesma memória; no PC
  são duas, e a camada copia e confere o que muda dos dois lados a cada quadro.

## Estado (25/11/2026)

Testado numa só máquina: Windows 11, RTX 4070 Ti SUPER 16 GB, 32 GB de RAM.

| | |
|---|---|
| Menus, abertura, cutscenes, prólogo na prisão | funcionam, com a imagem completa em 4K |
| Velocidade no gameplay | ~7–8 fps (ainda não é jogável de verdade) |
| Memória do PC | ~29 GB no pico: com 32 GB, feche todo o resto |
| Ray tracing | desligado (estoura a memória de um PC de 32 GB) |
| Perfil Series S (`-SeriesS`) | mais rápido (~9 fps), mas a imagem pisca |

## O que você precisa

- Windows 10 ou 11, 64 bits.
- Placa de vídeo com Direct3D 12, Shader Model 6.6 e mesh shaders (testado em NVIDIA RTX).
- 32 GB de RAM ou mais.
- [Rust](https://rustup.rs/) (a versão certa é instalada sozinha pelo `rust-toolchain.toml`).
- Visual Studio Build Tools com **Desktop development with C++** e um Windows SDK.
- Sua cópia do jogo, versão Xbox Series 1.0.2.1, extraída (a pasta com `game_scarlett_master.exe`).

## Passo a passo

1. **Baixe esta branch** (Code → Download ZIP, ou `git clone`).
2. **Compile** (na pasta do projeto, no PowerShell; leva alguns minutos na primeira vez):

   ```powershell
   cargo build --profile diag -p victory --features diagnostics --locked
   ```

3. **Configure:** copie `victory.example.toml` para `victory.toml` e aponte `game_dir` para a
   pasta do jogo. Os caminhos têm de ficar **fora** da pasta do projeto.
4. **Pré-traduza os shaders** (uma vez, ~2–3 minutos; evita travadas no começo):

   ```powershell
   .\jogar.ps1 -Jogo "D:\sua\pasta\do\jogo\game_scarlett_master.exe" -PreTraduzir
   ```

5. **Jogue:**

   ```powershell
   .\jogar.ps1 -Jogo "D:\sua\pasta\do\jogo\game_scarlett_master.exe"
   ```

   Use um controle. A primeira entrada no jogo demora: os pipelines de vídeo são montados
   enquanto você joga e ficam guardados para as próximas vezes.

## O que o `jogar.ps1` liga

São as configurações que deram melhor resultado nas medições (detalhes no `README.md` e nas
mensagens de commit):

- `VICTORY_INDIRECT_WINDOW=0`, `VICTORY_AMBIENT=0.6`: imagem correta e luz ambiente com sombra.
- `VICTORY_TEXTURE_NO_WAIT=1`, `VICTORY_LATE_WAIT=3`, `VICTORY_WIDE_BACK=1`,
  `VICTORY_CARD_BUFFERS_MIN_MB=1`, `VICTORY_CARD_BUFFERS_MB=1024`, `VICTORY_COPY_PLAIN=1`,
  `VICTORY_JOIN_COPIES=1`, `VICTORY_CARD_REHASH_RUNS=1`, `VICTORY_QUEUE2=1`,
  `VICTORY_CARD_MARKS=1`: menos cópias e conferências de memória entre CPU e placa.
- Com `-SeriesS`: perfil Series S com o upscaler do próprio jogo (`VICTORY_OWN_UPSCALER=1`).

## Créditos

- [VIctory](https://github.com/Marvisak/VIctory), de Marvisak e colaboradores: a base de tudo.
- [fxceless/victory-fork](https://github.com/fxceless/victory-fork): a base desta branch.
- Correções e ideias de: [ka1qux](https://github.com/ka1qux/victory-ka1qux) (luz ambiente),
  [Belobug](https://github.com/Belobug/VIctory-Belobug-AMD) (luz ambiente sombreada, clamps
  agrupados), [crt4ever](https://github.com/crt4ever/DumbFloridaMoron),
  [DexNom](https://github.com/DexNom/VIctory) via
  [Itsbatt/victory-unified](https://github.com/Itsbatt/victory-unified) (correções de shader),
  [vitutu1/victory-perf](https://github.com/vitutu1/victory-perf) e
  [ZigZagBoi1/ScarlettPort](https://github.com/ZigZagBoi1/ScarlettPort) (patches de
  compatibilidade), [Marvisak/VIctory](https://github.com/Marvisak/VIctory) (page marks).
- Licença GPL-3.0-or-later, como o VIctory.
- As mudanças desta branch foram feitas com ajuda de IA (Claude Code e Codex) e testadas em
  execuções reais na máquina descrita acima.
