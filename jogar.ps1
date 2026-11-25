# Roda o jogo com as configuracoes que deram melhor resultado nos testes (ver LEIA-ME.md).
#   .\jogar.ps1 -Jogo "D:\pasta\do\jogo\game_scarlett_master.exe"               # joga
#   .\jogar.ps1 -Jogo "D:\pasta\do\jogo\game_scarlett_master.exe" -PreTraduzir  # pre-traduz os shaders
#   .\jogar.ps1 -Jogo "..." -SeriesS                                             # perfil Series S (pisca)
param(
    [Parameter(Mandatory)][string]$Jogo,
    [switch]$PreTraduzir,
    [switch]$SeriesS
)
$ErrorActionPreference = 'Stop'
$raiz = $PSScriptRoot
$exe = Join-Path $raiz 'target\diag\victory.exe'
if (-not (Test-Path -LiteralPath $exe)) {
    throw "Compile primeiro: cargo build --profile diag -p victory --features diagnostics --locked"
}
if (-not (Test-Path -LiteralPath $Jogo)) { throw "Nao achei o jogo em: $Jogo" }
if (-not (Test-Path -LiteralPath (Join-Path $raiz 'victory.toml'))) {
    throw 'Copie victory.example.toml para victory.toml e aponte game_dir para a pasta do jogo.'
}
$env:VICTORY_CONFIG = Join-Path $raiz 'victory.toml'

if ($PreTraduzir) {
    $env:VICTORY_FILL_CACHE = '1'
    # Placas NVIDIA rodam waves de 32 lanes (como a camada detecta no jogo).
    $env:VICTORY_HOST_WAVE = '32'
    & $exe translate (Join-Path (Split-Path -Parent $Jogo) 'shaders.rpf')
    return
}

# Imagem correta e luz ambiente com sombra.
$env:VICTORY_INDIRECT_WINDOW = '0'
$env:VICTORY_AMBIENT = '0.6'
# Menos copias e conferencias de memoria entre a CPU e a placa (medidas, nao chutes).
$env:VICTORY_TEXTURE_NO_WAIT = '1'
$env:VICTORY_LATE_WAIT = '3'
$env:VICTORY_WIDE_BACK = '1'
$env:VICTORY_CARD_BUFFERS_MIN_MB = '1'
$env:VICTORY_CARD_BUFFERS_MB = '1024'
$env:VICTORY_COPY_PLAIN = '1'
$env:VICTORY_JOIN_COPIES = '1'
$env:VICTORY_CARD_REHASH_RUNS = '1'
$env:VICTORY_QUEUE2 = '1'
$env:VICTORY_CARD_MARKS = '1'

$argumentos = @('run', $Jogo, '--execute')
if ($SeriesS) {
    # A Series S escolhe o FSR, que ainda nao traduz: o upscaler do proprio jogo no lugar.
    $env:VICTORY_OWN_UPSCALER = '1'
    $argumentos += @('--console', 'series-s')
}
Push-Location $raiz
try { & $exe @argumentos } finally { Pop-Location }
