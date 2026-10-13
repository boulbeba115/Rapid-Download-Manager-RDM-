<#
.SYNOPSIS
    Create (or remove) a self-signed code-signing certificate for trying out
    the signed build on your own computer.

.DESCRIPTION
    A self-signed certificate proves nothing to anyone else: on other
    computers the publisher still shows as unknown and SmartScreen still
    warns. It is useful for exactly two things - testing the signing steps
    of package.ps1, and making Windows show a publisher name on computers
    where you deliberately install the certificate as trusted.

    For releases other people download you need a certificate issued by a
    certificate authority. See README, "Signing".

    Without switches: creates the certificate in your personal store and
    prints its thumbprint for RDM_SIGN_THUMBPRINT.

.PARAMETER Trust
    Also make THIS user account trust the certificate (Trusted Root
    Certification Authorities and Trusted Publishers of the current user).
    Windows asks you to confirm adding a root certificate. Only do this on
    your own computers, and remove it again with -Remove when done.

.PARAMETER Remove
    Delete the certificate and the trust entries this script created.
#>
param([switch]$Trust, [switch]$Remove)

$ErrorActionPreference = "Stop"
$subject = "CN=Rapid Download Manager (self-signed test)"
$stores = "Cert:\CurrentUser\My", "Cert:\CurrentUser\TrustedPublisher", "Cert:\CurrentUser\Root"

if ($Remove) {
    foreach ($store in $stores) {
        Get-ChildItem $store | Where-Object { $_.Subject -eq $subject } | ForEach-Object {
            Remove-Item $_.PSPath -Force
            Write-Host "removed $($_.Thumbprint) from $store"
        }
    }
    return
}

$certificate = Get-ChildItem $stores[0] -CodeSigningCert | Where-Object { $_.Subject -eq $subject -and $_.NotAfter -gt (Get-Date) } | Select-Object -First 1
if (-not $certificate) {
    $certificate = New-SelfSignedCertificate -Type CodeSigningCert -Subject $subject -CertStoreLocation $stores[0] `
        -KeyAlgorithm RSA -KeyLength 3072 -HashAlgorithm SHA256 -KeyExportPolicy NonExportable -NotAfter (Get-Date).AddYears(2)
    Write-Host "created $subject"
}

if ($Trust) {
    foreach ($name in "TrustedPublisher", "Root") {
        $store = New-Object System.Security.Cryptography.X509Certificates.X509Store($name, "CurrentUser")
        $store.Open("ReadWrite")
        # Only the public part goes into the trust stores.
        $store.Add((New-Object System.Security.Cryptography.X509Certificates.X509Certificate2(, $certificate.RawData)))
        $store.Close()
    }
    Write-Host "this user account now trusts the certificate (undo with -Remove)"
}

Write-Host ""
Write-Host "To sign a build with it:"
Write-Host "  `$env:RDM_SIGN_THUMBPRINT = `"$($certificate.Thumbprint)`""
Write-Host "  .\scripts\package.ps1 -Sign"
