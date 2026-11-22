param(
    [string]$Executable
)

$ErrorActionPreference = 'Stop'
$workspace = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).ProviderPath
$runtime = Join-Path $env:LOCALAPPDATA 'Hackson-Rust'
if (-not $Executable) {
    $Executable = @(
        (Join-Path $workspace 'apps/acceptance-rust/target/debug/hackson-acceptance.exe')
        (Join-Path $runtime 'acceptance-target/debug/hackson-acceptance.exe')
    ) | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
}
if (-not $Executable) {
    throw '请先构建 Rust 服务，或用 -Executable 指定已构建的程序。'
}
$Executable = (Resolve-Path -LiteralPath $Executable).ProviderPath
$secretFile = Join-Path $runtime 'secrets/b1x-media-key.dpapi'
$redbeanClientFile = Join-Path $runtime 'secrets/redbean-oauth-client.dpapi'
$redbeanPasswordFile = Join-Path $runtime 'secrets/redbean-platform-password.dpapi'
$contestTariff = Join-Path $runtime 'contest/cityline-tariff.xlsx'
$localConfig = Join-Path $workspace '.env.local'
$localValues = @{}
$loadedVariables = [System.Collections.Generic.List[string]]::new()

Push-Location -LiteralPath $workspace
try {
    if (Test-Path -LiteralPath $localConfig -PathType Leaf) {
        $lineNumber = 0
        foreach ($line in Get-Content -LiteralPath $localConfig -Encoding utf8) {
            $lineNumber++
            $entry = $line.Trim()
            if (-not $entry -or $entry.StartsWith('#')) { continue }
            if ($entry -cnotmatch '^((?:CITYLINE|ACCEPTANCE_RUST)_[A-Z0-9_]+)=(.*)$') {
                throw "本机环境配置第 $lineNumber 行须使用项目变量名及等号。"
            }
            $key = $Matches[1]
            $value = $Matches[2].Trim()
            if ($localValues.ContainsKey($key)) { throw "本机环境配置第 $lineNumber 行的变量重复。" }
            $localValues[$key] = $value
            if ($value -and -not [Environment]::GetEnvironmentVariable($key, 'Process')) {
                [Environment]::SetEnvironmentVariable($key, $value, 'Process')
                $loadedVariables.Add($key)
            }
        }
    }
    if (-not $env:CITYLINE_CONTEST_TARIFF -and (Test-Path -LiteralPath $contestTariff -PathType Leaf)) {
        $env:CITYLINE_CONTEST_TARIFF = $contestTariff
    }
    if (-not $env:ACCEPTANCE_RUST_MEDIA_MODEL_KEY -and $env:ACCEPTANCE_RUST_MEDIA_MODEL_URL -and
        $env:ACCEPTANCE_RUST_MEDIA_MODEL -and
        $env:ACCEPTANCE_RUST_MEDIA_MODEL_URL -ceq $localValues['ACCEPTANCE_RUST_MEDIA_MODEL_URL'] -and
        (Test-Path -LiteralPath $secretFile -PathType Leaf)) {
        $protectedKey = Get-Content -LiteralPath $secretFile -ErrorAction Stop | ConvertTo-SecureString
        $credential = [pscredential]::new('B1X', $protectedKey)
        $env:ACCEPTANCE_RUST_MEDIA_MODEL_KEY = $credential.GetNetworkCredential().Password
    }
    if (-not $env:CITYLINE_REDBEAN_OAUTH_CLIENT_SECRET -and $env:CITYLINE_REDBEAN_OAUTH_CLIENT_ID -and
        $env:CITYLINE_REDBEAN_OAUTH_ENDPOINT -ceq $localValues['CITYLINE_REDBEAN_OAUTH_ENDPOINT'] -and
        $env:CITYLINE_REDBEAN_OAUTH_CLIENT_ID -ceq $localValues['CITYLINE_REDBEAN_OAUTH_CLIENT_ID'] -and
        (Test-Path -LiteralPath $redbeanClientFile -PathType Leaf)) {
        $redbeanSecret = Get-Content -LiteralPath $redbeanClientFile -ErrorAction Stop | ConvertTo-SecureString
        $redbeanCredential = [pscredential]::new($env:CITYLINE_REDBEAN_OAUTH_CLIENT_ID, $redbeanSecret)
        $env:CITYLINE_REDBEAN_OAUTH_CLIENT_SECRET = $redbeanCredential.GetNetworkCredential().Password
    }
    if (-not $env:CITYLINE_REDBEAN_PASSWORD -and $env:CITYLINE_REDBEAN_USERNAME -and
        $env:CITYLINE_REDBEAN_USERNAME -ceq $localValues['CITYLINE_REDBEAN_USERNAME'] -and
        $env:CITYLINE_REDBEAN_ORIGIN -ceq $localValues['CITYLINE_REDBEAN_ORIGIN'] -and
        $env:CITYLINE_REDBEAN_OAUTH_ENDPOINT -ceq $localValues['CITYLINE_REDBEAN_OAUTH_ENDPOINT'] -and
        $env:CITYLINE_REDBEAN_OAUTH_CLIENT_ID -ceq $localValues['CITYLINE_REDBEAN_OAUTH_CLIENT_ID'] -and
        (Test-Path -LiteralPath $redbeanPasswordFile -PathType Leaf)) {
        $redbeanPassword = Get-Content -LiteralPath $redbeanPasswordFile -ErrorAction Stop | ConvertTo-SecureString
        $redbeanPlatformAccount = [pscredential]::new($env:CITYLINE_REDBEAN_USERNAME, $redbeanPassword)
        $env:CITYLINE_REDBEAN_PASSWORD = $redbeanPlatformAccount.GetNetworkCredential().Password
    }
    & $Executable
    if ($LASTEXITCODE -ne 0) {
        throw "城线服务退出，退出码 $LASTEXITCODE。"
    }
} finally {
    if ($credential) {
        Remove-Item Env:ACCEPTANCE_RUST_MEDIA_MODEL_KEY -ErrorAction SilentlyContinue
    }
    if ($redbeanCredential) {
        Remove-Item Env:CITYLINE_REDBEAN_OAUTH_CLIENT_SECRET -ErrorAction SilentlyContinue
    }
    if ($redbeanPlatformAccount) {
        Remove-Item Env:CITYLINE_REDBEAN_PASSWORD -ErrorAction SilentlyContinue
    }
    foreach ($key in $loadedVariables) {
        [Environment]::SetEnvironmentVariable($key, $null, 'Process')
    }
    Pop-Location
}
