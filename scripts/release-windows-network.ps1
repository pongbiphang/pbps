# A Docker wildcard IPAM subnet is not the host interface's source network.
function Get-FixtureSubnet([string]$Gateway, [object[]]$Addresses) {
    $address = [Net.IPAddress]::Parse($Gateway)
    if ($address.AddressFamily -ne [Net.Sockets.AddressFamily]::InterNetwork) {
        throw 'The fixture gateway must be IPv4'
    }
    $interfaces = @($Addresses | Where-Object IPAddress -eq $Gateway)
    if ($interfaces.Count -ne 1) { throw 'The fixture gateway must identify exactly one host address' }
    $prefix = [int]$interfaces[0].PrefixLength
    if ($prefix -lt 1 -or $prefix -gt 32) { throw 'The fixture source network must have a bounded IPv4 prefix' }
    $bytes = $address.GetAddressBytes()
    for ($i = 0; $i -lt 4; $i++) {
        $bits = [Math]::Min(8, [Math]::Max(0, $prefix - 8 * $i))
        $mask = [byte](256 - [Math]::Pow(2, 8 - $bits))
        $bytes[$i] = $bytes[$i] -band $mask
    }
    return "$($bytes -join '.')/$prefix"
}
