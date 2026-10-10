# Pure address selection checks; no administrator or firewall changes are needed.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. "$PSScriptRoot/release-windows-network.ps1"

$addresses = @(
    [pscustomobject]@{IPAddress='10.1.0.104'; PrefixLength=20},
    [pscustomobject]@{IPAddress='172.19.112.1'; PrefixLength=20}
)
# The measured Docker IPAM value was 0.0.0.0/0; only the actual gateway
# interface identifies the bounded network containing consumer 172.19.118.37.
if ((Get-FixtureSubnet '172.19.112.1' $addresses) -ne '172.19.112.0/20') {
    throw 'The measured native container subnet was not selected'
}
foreach ($case in @(@('192.168.9.129', 25, '192.168.9.128/25'), @('10.1.2.3', 32, '10.1.2.3/32'))) {
    $actual = Get-FixtureSubnet $case[0] @([pscustomobject]@{IPAddress=$case[0]; PrefixLength=$case[1]})
    if ($actual -ne $case[2]) { throw 'The interface prefix was not preserved' }
}
foreach ($invalid in @(
    @{Gateway='172.19.112.1'; Addresses=@()},
    @{Gateway='172.19.112.1'; Addresses=@($addresses[0])},
    @{Gateway='172.19.112.1'; Addresses=@($addresses[1], $addresses[1])},
    @{Gateway='172.19.112.1'; Addresses=@([pscustomobject]@{IPAddress='172.19.112.1'; PrefixLength=0})},
    @{Gateway='172.19.112.1'; Addresses=@([pscustomobject]@{IPAddress='172.19.112.1'; PrefixLength=33})},
    @{Gateway='::1'; Addresses=@([pscustomobject]@{IPAddress='::1'; PrefixLength=128})}
)) {
    $refused = $false
    try { Get-FixtureSubnet @invalid | Out-Null } catch { $refused = $true }
    if (-not $refused) { throw 'An absent, ambiguous or unbounded source network was accepted' }
}
Write-Output 'Native fixture subnet selection: passed'
