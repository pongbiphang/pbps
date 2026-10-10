#requires -Version 5.1
#requires -PSEdition Desktop
# Owned native engines for release qualification; never run on a shared host.
param(
    [Parameter(Mandatory)][ValidateSet('Start', 'Stop', 'Diagnose')][string]$Action,
    [Parameter(Mandatory)][string]$Root
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_OS -ne 'Windows') {
    throw 'This administrator fixture requires a disposable GitHub Windows runner'
}
$Root = [IO.Path]::GetFullPath($Root)
if (-not $Root.StartsWith([IO.Path]::GetFullPath($env:RUNNER_TEMP) + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Fixture root must be below RUNNER_TEMP'
}
$record = Join-Path $Root 'owner.json'

function Invoke-Checked([string]$File, [string[]]$Arguments) {
    & $File @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$File exited $LASTEXITCODE" }
}
function Save-Owner($Owner) {
    [IO.File]::WriteAllText($record, ($Owner | ConvertTo-Json -Depth 5))
}
function Export-Pem($Cert, [string]$Path) {
    $body = [Convert]::ToBase64String($Cert.RawData, [Base64FormattingOptions]::InsertLineBreaks)
    [IO.File]::WriteAllText($Path, "-----BEGIN CERTIFICATE-----`n$body`n-----END CERTIFICATE-----`n")
}
function Sql([string]$Instance, [string]$Statement) {
    $connection = [System.Data.SqlClient.SqlConnection]::new("Server=tcp:localhost,14333;Integrated Security=true;Encrypt=true;TrustServerCertificate=true;Connection Timeout=5")
    try {
        $connection.Open()
        $command = $connection.CreateCommand()
        $command.CommandTimeout = 30
        $command.CommandText = $Statement
        return $command.ExecuteScalar()
    } finally { $connection.Dispose() }
}

if ($Action -eq 'Diagnose') {
    if (-not (Test-Path $record)) { throw 'Missing owned fixture record' }
    $owner = Get-Content -Raw $record | ConvertFrom-Json
    # Read only bounded fixture topology, never credentials or certificate keys.
    Get-NetTCPConnection -State Listen | Where-Object LocalPort -in @(15432, 14333) |
        Select-Object LocalAddress, LocalPort, OwningProcess | Format-Table | Out-String | Write-Output
    & docker network inspect nat --format '{{json .IPAM.Config}}'
    if ($LASTEXITCODE) { throw 'Cannot inspect fixture NAT network' }
    Get-NetIPAddress -AddressFamily IPv4 |
        Select-Object InterfaceAlias, IPAddress, PrefixLength | Format-Table | Out-String | Write-Output
    Get-NetFirewallProfile |
        Select-Object Name, Enabled, DefaultInboundAction, AllowInboundRules, AllowLocalFirewallRules |
        Format-Table | Out-String | Write-Output
    foreach ($name in $owner.firewalls) {
        $rule = Get-NetFirewallRule -Name $name -ErrorAction Stop
        $rule | Select-Object Name, Enabled, Direction, Action, Profile, PolicyStoreSourceType |
            Format-List | Out-String | Write-Output
        $rule | Get-NetFirewallAddressFilter | Select-Object LocalAddress, RemoteAddress |
            Format-List | Out-String | Write-Output
        $rule | Get-NetFirewallPortFilter | Select-Object Protocol, LocalPort, RemotePort |
            Format-List | Out-String | Write-Output
    }
    foreach ($address in @('127.0.0.1', ((& docker network inspect nat | ConvertFrom-Json)[0].IPAM.Config[0].Gateway))) {
        foreach ($port in @(15432, 14333)) {
            $client = [Net.Sockets.TcpClient]::new()
            try {
                $pending = $client.BeginConnect($address, $port, $null, $null)
                if (-not $pending.AsyncWaitHandle.WaitOne(5000)) { throw 'TCP probe timed out' }
                $client.EndConnect($pending)
                Write-Output "Fixture host TCP probe ${address}:${port}: connected"
            } catch { Write-Output "Fixture host TCP probe ${address}:${port}: $($_.Exception.Message)" }
            finally { $client.Dispose() }
        }
    }
    $pgLog = Join-Path $Root 'pg.log'
    if (Test-Path $pgLog) {
        Get-Content $pgLog -Tail 40 | ForEach-Object { $_.Replace('Pbps!Test12345', '[redacted]') }
    }
    return
}

if ($Action -eq 'Stop') {
    if (-not (Test-Path $record)) { throw 'Missing owned fixture record' }
    $owner = Get-Content -Raw $record | ConvertFrom-Json
    $errors = [Collections.Generic.List[string]]::new()
    if ($owner.pgStarted) {
        try { Invoke-Checked (Join-Path $owner.pgBin 'pg_ctl.exe') @('-D', (Join-Path $Root 'pgdata'), '-m', 'immediate', '-w', 'stop') }
        catch { $errors.Add($_.Exception.Message) }
    }
    if ($owner.installStarted) {
        try {
            $arguments = @('/Q', '/ACTION=Uninstall', '/FEATURES=SQLENGINE', "/INSTANCENAME=$($owner.instance)")
            $process = Start-Process -FilePath (Join-Path $Root 'media\setup.exe') -ArgumentList $arguments -Wait -PassThru
            if ($process.ExitCode -notin @(0, 3010)) { throw "SQL uninstall exited $($process.ExitCode)" }
        } catch { $errors.Add($_.Exception.Message) }
    }
    foreach ($name in $owner.firewalls) {
        try { Get-NetFirewallRule -Name $name -ErrorAction SilentlyContinue | Remove-NetFirewallRule }
        catch { $errors.Add($_.Exception.Message) }
    }
    foreach ($thumbprint in $owner.certificates) {
        try { Remove-Item "Cert:\LocalMachine\My\$thumbprint" -DeleteKey -ErrorAction Stop }
        catch { $errors.Add($_.Exception.Message) }
    }
    if ($errors.Count) { throw ($errors -join '; ') }
    # Keep the ownership record if deleting any owned staging/data file fails.
    Get-ChildItem -LiteralPath $Root -Force | Where-Object Name -ne 'owner.json' | Remove-Item -Recurse -Force
    Remove-Item $record
    Remove-Item $Root
    return
}

if (Test-Path $Root) { throw 'Refusing to reuse a fixture directory' }
New-Item -ItemType Directory $Root | Out-Null
$instance = 'PBPS' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
$owner = @{ instance=$instance; pgBin=$env:PGBIN; pgStarted=$false; installStarted=$false; certificates=@(); firewalls=@() }
Save-Owner $owner
try {
    if (-not $env:PGBIN -or -not (Test-Path (Join-Path $env:PGBIN 'initdb.exe'))) { throw 'Native PostgreSQL tools are required' }
    if (Get-Service -Name "MSSQL`$$instance" -ErrorAction SilentlyContinue) { throw 'Instance name already exists' }
    foreach ($port in @(15432, 14333)) {
        if (Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue) { throw "Fixture port $port is already occupied" }
    }
    $installer = Join-Path $Root 'SQLEXPR_x64_ENU.exe'
    Invoke-WebRequest -UseBasicParsing -Uri 'https://download.microsoft.com/download/3/8/d/38de7036-2433-4207-8eae-06e247e17b25/SQLEXPR_x64_ENU.exe' -OutFile $installer
    if ((Get-FileHash $installer -Algorithm SHA256).Hash.ToLowerInvariant() -ne '2e61c8bbde6021f9026c54ad9db4bbb1227e68761d4c00a6a50a2c70fe7afe05') { throw 'SQL Server media checksum mismatch' }
    if ((Get-AuthenticodeSignature $installer).Status -ne 'Valid') { throw 'SQL Server media signature is not valid' }
    $media = Join-Path $Root 'media'
    $extract = Start-Process $installer -ArgumentList @('/Q', "/X:`"$media`"") -Wait -PassThru
    if ($extract.ExitCode -ne 0) { throw "SQL extraction exited $($extract.ExitCode)" }
    $owner.installStarted = $true
    Save-Owner $owner
    $setup = @('/Q', '/ACTION=Install', '/FEATURES=SQLENGINE', "/INSTANCENAME=$instance", "/INSTANCEDIR=`"$Root\sql`"",
        '/IACCEPTSQLSERVERLICENSETERMS', '/SUPPRESSPRIVACYSTATEMENTNOTICE', '/UpdateEnabled=False',
        '/TCPENABLED=1', '/NPENABLED=0', '/SECURITYMODE=SQL', '/SAPWD=Pbps!Test12345',
        "/SQLSYSADMINACCOUNTS=`"$env:USERDOMAIN\$env:USERNAME`"")
    $install = Start-Process (Join-Path $media 'setup.exe') -ArgumentList $setup -Wait -PassThru
    if ($install.ExitCode -notin @(0, 3010)) { throw "SQL installation exited $($install.ExitCode)" }

    # Only the personal certificate store is used for the server's private key;
    # the client trust store is never modified. Clients use explicit PEM roots.
    $until = (Get-Date).AddDays(2)
    $ca = New-SelfSignedCertificate -Subject 'CN=pbps release root' -Type Custom -KeyUsage CertSign, CRLSign -TextExtension @('2.5.29.19={critical}{text}ca=1') -CertStoreLocation Cert:\LocalMachine\My -NotAfter $until
    $owner.certificates += $ca.Thumbprint; Save-Owner $owner
    $untrusted = New-SelfSignedCertificate -Subject 'CN=pbps unrelated root' -Type Custom -KeyUsage CertSign, CRLSign -TextExtension @('2.5.29.19={critical}{text}ca=1') -CertStoreLocation Cert:\LocalMachine\My -NotAfter $until
    $owner.certificates += $untrusted.Thumbprint; Save-Owner $owner
    $peer = New-SelfSignedCertificate -Subject "CN=$env:COMPUTERNAME" -DnsName @($env:COMPUTERNAME, 'pbps-db', 'localhost') -Signer $ca -Type Custom -KeySpec KeyExchange -Provider 'Microsoft RSA SChannel Cryptographic Provider' -KeyExportPolicy Exportable -KeyUsage DigitalSignature, KeyEncipherment -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.1') -CertStoreLocation Cert:\LocalMachine\My -NotAfter $until
    $owner.certificates += $peer.Thumbprint; Save-Owner $owner
    Export-Pem $ca (Join-Path $Root 'ca.pem')
    Export-Pem $untrusted (Join-Path $Root 'untrusted.pem')
    Export-Pem $peer (Join-Path $Root 'peer.pem')
    $pfx = Join-Path $Root 'peer.pfx'
    Export-PfxCertificate -Cert $peer -FilePath $pfx -Password (ConvertTo-SecureString 'pbps-fixture' -AsPlainText -Force) | Out-Null
    Invoke-Checked 'openssl' @('pkcs12', '-in', $pfx, '-passin', 'pass:pbps-fixture', '-nodes', '-nocerts', '-out', (Join-Path $Root 'peer.key'))
    # SQL Server's CAPI key needs its persisted container ACL. Desktop's
    # PrivateKey preserves that provider; GetRSAPrivateKey may return a CNG wrapper.
    $rsa = [Security.Cryptography.RSACryptoServiceProvider]$peer.PrivateKey
    $keyPath = Join-Path $env:ProgramData "Microsoft\Crypto\RSA\MachineKeys\$($rsa.CspKeyContainerInfo.UniqueKeyContainerName)"
    $acl = Get-Acl $keyPath
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new("NT SERVICE\MSSQL`$$instance", 'Read', 'Allow'))
    Set-Acl $keyPath $acl
    $id = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Microsoft SQL Server\Instance Names\SQL').$instance
    $net = "HKLM:\SOFTWARE\Microsoft\Microsoft SQL Server\$id\MSSQLServer\SuperSocketNetLib"
    Set-ItemProperty $net Certificate $peer.Thumbprint.ToLowerInvariant()
    Set-ItemProperty $net ForceEncryption 1
    Set-ItemProperty "$net\Tcp\IPAll" TcpDynamicPorts ''
    Set-ItemProperty "$net\Tcp\IPAll" TcpPort '14333'
    Restart-Service "MSSQL`$$instance"
    $version = Sql $instance 'SELECT @@VERSION'
    Sql $instance 'CREATE DATABASE pbps_release' | Out-Null

    $pgdata = Join-Path $Root 'pgdata'
    $password = Join-Path $Root 'password'
    [IO.File]::WriteAllText($password, "Pbps!Test12345`n")
    Invoke-Checked (Join-Path $env:PGBIN 'initdb.exe') @('-D', $pgdata, '-U', 'postgres', '--auth-host=scram-sha-256', '--auth-local=trust', "--pwfile=$password", '--encoding=UTF8')
    Remove-Item $password
    $key = (Join-Path $Root 'peer.key').Replace('\', '/')
    $cert = (Join-Path $Root 'peer.pem').Replace('\', '/')
    Add-Content (Join-Path $pgdata 'postgresql.conf') "`nlisten_addresses='*'`nport=15432`nssl=on`nssl_key_file='$key'`nssl_cert_file='$cert'"
    Add-Content (Join-Path $pgdata 'pg_hba.conf') 'hostssl all all 0.0.0.0/0 scram-sha-256'
    # The owned cluster is recorded before launch so failure cannot evade stop.
    $owner.pgStarted = $true; Save-Owner $owner
    Invoke-Checked (Join-Path $env:PGBIN 'pg_ctl.exe') @('-D', $pgdata, '-l', (Join-Path $Root 'pg.log'), '-w', 'start')
    $env:PGPASSWORD = 'Pbps!Test12345'
    Invoke-Checked (Join-Path $env:PGBIN 'psql.exe') @('-h', 'localhost', '-p', '15432', '-U', 'postgres', '-v', 'ON_ERROR_STOP=1', '-c', 'CREATE DATABASE pbps_release')
    $pgVersion = & (Join-Path $env:PGBIN 'psql.exe') -h localhost -p 15432 -U postgres -Atc 'SELECT version()'
    if ($LASTEXITCODE -ne 0) { throw 'Native PostgreSQL version query failed' }
    Remove-Item Env:\PGPASSWORD
    $nat = (& docker network inspect nat | ConvertFrom-Json)[0]
    if ($LASTEXITCODE -ne 0) { throw 'Native Windows Docker NAT is required' }
    $gateway = $nat.IPAM.Config[0].Gateway
    $subnet = $nat.IPAM.Config[0].Subnet
    foreach ($port in @(15432, 14333)) {
        $name = "$instance-$port"
        $owner.firewalls += $name; Save-Owner $owner
        New-NetFirewallRule -Name $name -DisplayName $name -Direction Inbound -Action Allow -Protocol TCP -LocalPort $port -RemoteAddress $subnet | Out-Null
    }
    $fixture = @{ trust=$Root; gateway=$gateway;
       postgres=@{port=15432; wrong_host=$gateway; version=$pgVersion};
       mssql=@{port=14333; wrong_host=$gateway; version=$version} } |
        ConvertTo-Json -Depth 5
    # Windows PowerShell's UTF8 Set-Content adds a BOM; Python reads plain UTF-8.
    [IO.File]::WriteAllText((Join-Path $Root 'fixture.json'), $fixture)
} catch {
    Write-Error -ErrorAction Continue $_
    # CI must retain the engine's reason before the owned instance is removed.
    # Never upload the fixture directory: it also holds private keys and media.
    try {
        $logs = @((Join-Path $Root 'pg.log'), (Join-Path $env:ProgramFiles 'Microsoft SQL Server\160\Setup Bootstrap\Log\Summary.txt'))
        if (Test-Path (Join-Path $Root 'sql')) {
            $logs += @(Get-ChildItem (Join-Path $Root 'sql') -Recurse -Filter ERRORLOG -File | Select-Object -ExpandProperty FullName)
        }
        foreach ($log in $logs) {
            if (Test-Path $log) {
                Write-Output "Fixture diagnostic: $log"
                Get-Content $log -Tail 80 | ForEach-Object { $_.Replace('Pbps!Test12345', '[redacted]') }
            }
        }
    } catch { Write-Warning "Fixture diagnostics could not be read: $_" }
    throw 'Native Windows fixture setup failed; the always-run cleanup must read owner.json'
}
