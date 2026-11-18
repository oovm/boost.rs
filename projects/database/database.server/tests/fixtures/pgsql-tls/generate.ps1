# Generates the test-only PostgreSQL TLS material in this directory.
#
# The fixtures are a local CA (`ca.pem`) and a `localhost` / `127.0.0.1` leaf
# (`server.pem` + `server.key`). Nothing here is secret: the material is checked
# in so TLS handshakes stay reproducible without an OpenSSL install.
#
# Run from this directory with PowerShell 7+:
#   pwsh -File generate.ps1
$ErrorActionPreference = 'Stop'

$notBefore = [DateTimeOffset]::new(2020, 1, 1, 0, 0, 0, [TimeSpan]::Zero)
$notAfter = [DateTimeOffset]::new(2045, 1, 1, 0, 0, 0, [TimeSpan]::Zero)

function Convert-ToPem([string] $label, [byte[]] $der) {
    $base64 = [Convert]::ToBase64String($der)
    $lines = for ($index = 0; $index -lt $base64.Length; $index += 64) {
        $base64.Substring($index, [Math]::Min(64, $base64.Length - $index))
    }
    return "-----BEGIN $label-----`n" + ($lines -join "`n") + "`n-----END $label-----`n"
}

$caKey = [System.Security.Cryptography.RSA]::Create(2048)
$caRequest = [System.Security.Cryptography.X509Certificates.CertificateRequest]::new(
    'CN=boost-pg-tls-test-ca',
    $caKey,
    [System.Security.Cryptography.HashAlgorithmName]::SHA256,
    [System.Security.Cryptography.RSASignaturePadding]::Pkcs1)
$caRequest.CertificateExtensions.Add(
    [System.Security.Cryptography.X509Certificates.X509BasicConstraintsExtension]::new($true, $false, 0, $true))
$caRequest.CertificateExtensions.Add(
    [System.Security.Cryptography.X509Certificates.X509KeyUsageExtension]::new(
        [System.Security.Cryptography.X509Certificates.X509KeyUsageFlags]::KeyCertSign -bor
        [System.Security.Cryptography.X509Certificates.X509KeyUsageFlags]::CrlSign,
        $true))
$caCertificate = $caRequest.CreateSelfSigned($notBefore, $notAfter)

$leafKey = [System.Security.Cryptography.RSA]::Create(2048)
$leafRequest = [System.Security.Cryptography.X509Certificates.CertificateRequest]::new(
    'CN=localhost',
    $leafKey,
    [System.Security.Cryptography.HashAlgorithmName]::SHA256,
    [System.Security.Cryptography.RSASignaturePadding]::Pkcs1)
$leafRequest.CertificateExtensions.Add(
    [System.Security.Cryptography.X509Certificates.X509BasicConstraintsExtension]::new($false, $false, 0, $true))
$leafRequest.CertificateExtensions.Add(
    [System.Security.Cryptography.X509Certificates.X509KeyUsageExtension]::new(
        [System.Security.Cryptography.X509Certificates.X509KeyUsageFlags]::DigitalSignature -bor
        [System.Security.Cryptography.X509Certificates.X509KeyUsageFlags]::KeyEncipherment,
        $true))
$usages = [System.Security.Cryptography.OidCollection]::new()
$usages.Add([System.Security.Cryptography.Oid]::new('1.3.6.1.5.5.7.3.1')) | Out-Null
$leafRequest.CertificateExtensions.Add(
    [System.Security.Cryptography.X509Certificates.X509EnhancedKeyUsageExtension]::new($usages, $false))
$names = [System.Security.Cryptography.X509Certificates.SubjectAlternativeNameBuilder]::new()
$names.AddDnsName('localhost')
$names.AddIpAddress([System.Net.IPAddress]::Parse('127.0.0.1'))
$leafRequest.CertificateExtensions.Add($names.Build($false))

$serial = [byte[]]::new(8)
[System.Security.Cryptography.RandomNumberGenerator]::Fill($serial)
$serial[0] = $serial[0] -band 0x7F
$leafCertificate = $leafRequest.Create($caCertificate, $notBefore, $notAfter, $serial)

$directory = Split-Path -Parent $MyInvocation.MyCommand.Path
[IO.File]::WriteAllText((Join-Path $directory 'ca.pem'), (Convert-ToPem 'CERTIFICATE' $caCertificate.Export([System.Security.Cryptography.X509Certificates.X509ContentType]::Cert)))
[IO.File]::WriteAllText((Join-Path $directory 'server.pem'), (Convert-ToPem 'CERTIFICATE' $leafCertificate.Export([System.Security.Cryptography.X509Certificates.X509ContentType]::Cert)))
[IO.File]::WriteAllText((Join-Path $directory 'server.key'), (Convert-ToPem 'PRIVATE KEY' $leafKey.ExportPkcs8PrivateKey()))
