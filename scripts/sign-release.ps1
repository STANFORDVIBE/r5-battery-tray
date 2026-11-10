# Подписывает R5BatteryTray.exe закрытым ключом релизов -> R5BatteryTray.exe.sig (ECDSA P-256 + SHA-256).
# Виджет ставит обновление, только если подпись сходится с открытым ключом release.pub, вшитым в exe.
# Ключ хранится зашифрованным паролем (PBKDF2-SHA256 600 000 раундов + AES-256): украденный файл без пароля бесполезен.
#   .\scripts\sign-release.ps1           — подписать релиз (спросит пароль)
#   .\scripts\sign-release.ps1 -Protect  — зашифровать паролем ключ, который пока лежит открытым
#   .\scripts\sign-release.ps1 -NewKey   — создать ключ (только если его ещё нет), потом пересобрать exe
param([switch]$NewKey, [switch]$Protect, [string]$KeyFile = (Join-Path $env:USERPROFILE '.r5batterytray\release.key'))
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Core
$root = Split-Path $PSScriptRoot
$Rounds = 600000
# зашифрованный файл: KBK1 | соль 16 | IV 16 | шифротекст

function Read-Password([string]$prompt) {
    $s = Read-Host -Prompt $prompt -AsSecureString
    $p = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($s)
    try { [Text.Encoding]::UTF8.GetBytes([Runtime.InteropServices.Marshal]::PtrToStringBSTR($p)) }
    finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($p) }
}

function New-Password {
    $a = Read-Password 'Придумай пароль для ключа (от 12 символов)'
    if ($a.Length -lt 12) { throw 'Пароль короче 12 символов' }
    $b = Read-Password 'Повтори пароль'
    if ([Convert]::ToBase64String($a) -ne [Convert]::ToBase64String($b)) { throw 'Пароли не совпали' }
    , $a
}

function Get-Aes([byte[]]$password, [byte[]]$salt, [byte[]]$iv) {
    $kdf = New-Object Security.Cryptography.Rfc2898DeriveBytes($password, $salt, $Rounds, [Security.Cryptography.HashAlgorithmName]::SHA256)
    $aes = [Security.Cryptography.Aes]::Create()
    $aes.Key = $kdf.GetBytes(32); $aes.IV = $iv
    $aes
}

function Protect-Key([byte[]]$plain, [byte[]]$password) {
    $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
    $salt = New-Object byte[] 16; $iv = New-Object byte[] 16
    $rng.GetBytes($salt); $rng.GetBytes($iv)
    $enc = (Get-Aes $password $salt $iv).CreateEncryptor().TransformFinalBlock($plain, 0, $plain.Length)
    , [byte[]]([Text.Encoding]::ASCII.GetBytes('KBK1') + $salt + $iv + $enc)
}

function Test-Encrypted([byte[]]$data) { $data.Length -gt 36 -and [Text.Encoding]::ASCII.GetString($data, 0, 4) -eq 'KBK1' }

function Unprotect-Key([byte[]]$data, [byte[]]$password) {
    try {
        $aes = Get-Aes $password ([byte[]]$data[4..19]) ([byte[]]$data[20..35])
        , $aes.CreateDecryptor().TransformFinalBlock($data, 36, $data.Length - 36)
    } catch { throw 'Неверный пароль' }
}

# Ключ подходит к release.pub, вшитому в exe? Заодно ловит неверный пароль (расшифруется мусор).
function Test-MatchesPub([byte[]]$plain) {
    try { $k = [Security.Cryptography.CngKey]::Import($plain, [Security.Cryptography.CngKeyBlobFormat]::EccPrivateBlob) }
    catch { return $false }
    $pub = [IO.File]::ReadAllBytes((Join-Path $root 'release.pub'))
    [Convert]::ToBase64String($k.Export([Security.Cryptography.CngKeyBlobFormat]::EccPublicBlob)) -eq [Convert]::ToBase64String($pub)
}

if ($NewKey) {
    if (Test-Path $KeyFile) { throw "Ключ уже есть: $KeyFile. Старые версии примут обновления только с ним — не перезаписываю." }
    $password = New-Password
    $p = New-Object Security.Cryptography.CngKeyCreationParameters
    $p.ExportPolicy = [Security.Cryptography.CngExportPolicies]::AllowPlaintextExport
    # без имени — ключ только в памяти, не в хранилище Windows
    $k = [Security.Cryptography.CngKey]::Create([Security.Cryptography.CngAlgorithm]::ECDsaP256, [NullString]::Value, $p)
    New-Item -ItemType Directory -Force (Split-Path $KeyFile) | Out-Null
    [IO.File]::WriteAllBytes($KeyFile, (Protect-Key $k.Export([Security.Cryptography.CngKeyBlobFormat]::EccPrivateBlob) $password))
    [IO.File]::WriteAllBytes((Join-Path $root 'release.pub'), $k.Export([Security.Cryptography.CngKeyBlobFormat]::EccPublicBlob))
    "Новый ключ (под паролем): $KeyFile, открытый — release.pub. Пересобери exe."
    return
}

if (-not (Test-Path $KeyFile)) { throw "Нет ключа $KeyFile. Без него обновления не подписать." }
$data = [IO.File]::ReadAllBytes($KeyFile)

if ($Protect) {
    if (Test-Encrypted $data) { 'Ключ уже под паролем.'; return }
    if (-not (Test-MatchesPub $data)) { throw 'Ключ не подходит к release.pub — не трогаю.' }
    $password = New-Password
    $enc = Protect-Key $data $password
    $check = Unprotect-Key $enc $password  # проверка до замены, чтобы не остаться без ключа
    if ([Convert]::ToBase64String($check) -ne [Convert]::ToBase64String($data)) { throw 'Проверка шифрования не прошла — файл не тронут.' }
    [IO.File]::WriteAllBytes($KeyFile, $enc)
    "Ключ зашифрован паролем: $KeyFile"
    return
}

if (-not (Test-Encrypted $data)) { throw 'Ключ лежит без пароля. Сначала: .\scripts\sign-release.ps1 -Protect' }
$plain = Unprotect-Key $data (Read-Password 'Пароль ключа подписи')
if (-not (Test-MatchesPub $plain)) { throw 'Неверный пароль' }
$key = [Security.Cryptography.CngKey]::Import($plain, [Security.Cryptography.CngKeyBlobFormat]::EccPrivateBlob)
[Array]::Clear($plain, 0, $plain.Length)
$ecdsa = New-Object Security.Cryptography.ECDsaCng($key)
$exe = Join-Path $root 'R5BatteryTray.exe'
$exeData = [IO.File]::ReadAllBytes($exe)
$sig = $ecdsa.SignData($exeData)  # SHA-256, подпись r||s 64 байта — тот формат, что ждёт BCryptVerifySignature
$pubKey = [Security.Cryptography.CngKey]::Import([IO.File]::ReadAllBytes((Join-Path $root 'release.pub')), [Security.Cryptography.CngKeyBlobFormat]::EccPublicBlob)
if (-not (New-Object Security.Cryptography.ECDsaCng($pubKey)).VerifyData($exeData, $sig)) { throw 'Подпись не сходится с release.pub' }
[IO.File]::WriteAllBytes("$exe.sig", $sig)
"Подписано: $exe.sig"
