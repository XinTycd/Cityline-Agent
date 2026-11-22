param(
    [Parameter(Mandatory)][string]$Workspace,
    [Parameter(Mandatory)][string]$Runtime
)

$ErrorActionPreference = 'Stop'
$keys = @(Get-Content -LiteralPath (Join-Path $Workspace '.env.example') -Encoding utf8 | ForEach-Object {
    if ($_ -cmatch '^((?:CITYLINE|ACCEPTANCE_RUST)_[A-Z0-9_]+)=') { $Matches[1] }
})
$values = [ordered]@{}
foreach ($key in $keys) {
    $value = [Environment]::GetEnvironmentVariable($key, 'Process')
    if (-not [string]::IsNullOrWhiteSpace($value)) {
        if ($value -match '[\r\n\x00]') { throw "配置 $key 包含换行或空字符。" }
        $values[$key] = $value
    }
}

foreach ($key in @(
    'CITYLINE_NEWCLOUD_ORIGIN', 'CITYLINE_REDBEAN_ORIGIN', 'CITYLINE_CONTEST_TARIFF',
    'CITYLINE_REDBEAN_OAUTH_CLIENT_ID', 'CITYLINE_REDBEAN_OAUTH_CLIENT_SECRET',
    'CITYLINE_REDBEAN_USERNAME', 'CITYLINE_REDBEAN_PASSWORD',
    'ACCEPTANCE_RUST_MEDIA_MODEL_URL', 'ACCEPTANCE_RUST_MEDIA_MODEL',
    'ACCEPTANCE_RUST_MEDIA_MODEL_KEY'
)) {
    if (-not $values[$key]) { throw "当前运行配置缺少 $key。请用平时启动工作台的 Windows 账号导出。" }
}

$files = [ordered]@{
    CITYLINE_CONTEST_TARIFF = 'tariff.xlsx'
    CITYLINE_OIDC_CA_FILE = 'oidc-ca.pem'
    CITYLINE_FEISHU_CA_FILE = 'feishu-ca.pem'
}
$sources = @{}
foreach ($key in $files.Keys) {
    if ($values[$key]) {
        $sources[$key] = (Resolve-Path -LiteralPath $values[$key]).ProviderPath
        $values[$key] = "/etc/cityline/$($files[$key])"
    }
}
$database = if ($values['ACCEPTANCE_RUST_DATA']) { $values['ACCEPTANCE_RUST_DATA'] } else { Join-Path $Workspace '.operations-data/cityline.sqlite' }
$database = (Resolve-Path -LiteralPath $database).ProviderPath
$values['ACCEPTANCE_RUST_PORT'] = '3412'
$values['ACCEPTANCE_RUST_DATA'] = '/var/lib/cityline/cityline.sqlite'
$values['CITYLINE_PUBLIC_ORIGIN'] = ''
foreach ($key in 'CITYLINE_NEWCLOUD_SESSION', 'CITYLINE_REDBEAN_ACCESS_TOKEN') {
    $values.Remove($key)
}

$folder = Join-Path $Runtime ("ubuntu-transfer-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $folder | Out-Null
$owner = [Security.Principal.WindowsIdentity]::GetCurrent().User
$permissions = Get-Acl -LiteralPath $folder
$permissions.SetAccessRuleProtection($true, $false)
$permissions.SetAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
    $owner, 'FullControl', 'ContainerInherit, ObjectInherit', 'None', 'Allow'
))
Set-Acl -LiteralPath $folder -AclObject $permissions

$settingsScript = Join-Path $PSScriptRoot 'transfer-settings.py'
python $settingsScript export $database (Join-Path $folder 'settings.json')
if ($LASTEXITCODE -ne 0) { throw '工作台配置导出失败。' }
foreach ($key in $sources.Keys) {
    Copy-Item -LiteralPath $sources[$key] -Destination (Join-Path $folder $files[$key])
}
$lines = foreach ($key in $values.Keys) { "$key=$($values[$key])" }
[IO.File]::WriteAllText((Join-Path $folder 'cityline.env'), ($lines -join "`n") + "`n", [Text.UTF8Encoding]::new($false))

Write-Output $folder
