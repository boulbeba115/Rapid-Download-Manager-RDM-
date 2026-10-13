<#
.SYNOPSIS
    Authenticode-sign one or more files (SHA-256, time-stamped).

.DESCRIPTION
    Used by package.ps1 -Sign for the executables, and by the installer
    script for the installer and the uninstaller inside it.

    Which certificate to use comes from the environment, so that no secret
    ever sits in the repository or on a command line:

      RDM_SIGN_THUMBPRINT     thumbprint of a code-signing certificate in the
                              Windows certificate store (Current User or Local
                              Machine, "Personal"). This is also how a
                              certificate on a USB token or in a cloud HSM is
                              addressed once its driver has put it in the store.
      RDM_SIGN_PFX            path of a .pfx file, instead of the store
      RDM_SIGN_PFX_PASSWORD   its password, if it has one
      RDM_SIGN_TIMESTAMP      time-stamp server (default: DigiCert's);
                              "none" to sign without one (the signature then
                              stops being valid when the certificate expires)

    Windows only shows a publisher name, and SmartScreen only builds
    reputation, for a certificate issued by a certificate authority that
    Windows trusts. See README, "Signing".

.EXAMPLE
    $env:RDM_SIGN_THUMBPRINT = "0123456789ABCDEF0123456789ABCDEF01234567"
    .\scripts\sign.ps1 dist\stage\rdm.exe
#>
param([Parameter(Mandatory = $true, ValueFromRemainingArguments = $true)][string[]]$Path)

$ErrorActionPreference = "Stop"

function Get-SigningCertificate {
    if ($env:RDM_SIGN_PFX) {
        if (-not (Test-Path $env:RDM_SIGN_PFX)) { throw "RDM_SIGN_PFX: '$env:RDM_SIGN_PFX' does not exist." }
        $file = (Resolve-Path $env:RDM_SIGN_PFX).Path
        return New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($file, [string]$env:RDM_SIGN_PFX_PASSWORD)
    }
    if (-not $env:RDM_SIGN_THUMBPRINT) {
        throw "No signing certificate configured. Set RDM_SIGN_THUMBPRINT (or RDM_SIGN_PFX). See README, 'Signing'."
    }
    $thumbprint = ($env:RDM_SIGN_THUMBPRINT -replace '[^0-9A-Fa-f]', '').ToUpper()
    foreach ($store in "Cert:\CurrentUser\My", "Cert:\LocalMachine\My") {
        $found = Get-ChildItem $store -CodeSigningCert -ErrorAction SilentlyContinue | Where-Object { $_.Thumbprint -eq $thumbprint }
        if ($found) { return @($found)[0] }
    }
    throw "No code-signing certificate with thumbprint $thumbprint in the certificate store (Personal)."
}

$certificate = Get-SigningCertificate
if (-not $certificate.HasPrivateKey) { throw "The certificate '$($certificate.Subject)' has no private key here; it cannot sign." }
if ($certificate.NotAfter -lt (Get-Date)) { throw "The certificate '$($certificate.Subject)' expired on $($certificate.NotAfter)." }

$timestamp = if ($env:RDM_SIGN_TIMESTAMP) { $env:RDM_SIGN_TIMESTAMP } else { "http://timestamp.digicert.com" }

foreach ($file in $Path) {
    if (-not (Test-Path $file)) { throw "Cannot sign '$file': no such file." }
    $arguments = @{ FilePath = $file; Certificate = $certificate; HashAlgorithm = "SHA256"; IncludeChain = "NotRoot" }
    if ($timestamp -ne "none") { $arguments.TimestampServer = $timestamp }
    $signature = Set-AuthenticodeSignature @arguments

    # "UnknownError" is what a correct signature reports when this computer does
    # not trust the certificate's issuer, i.e. a self-signed test certificate.
    if (-not $signature.SignerCertificate -or $signature.Status -notin "Valid", "UnknownError") {
        throw "Signing '$file' failed: $($signature.Status) - $($signature.StatusMessage)"
    }
    $note = if ($signature.Status -eq "Valid") { "trusted on this computer" } else { "NOT trusted on this computer (self-signed?)" }
    Write-Host ("signed {0}  [{1}; {2}]" -f (Split-Path -Leaf $file), $certificate.Subject, $note)
}
