<#
.SYNOPSIS
  Builds the SnapCap installer and publishes a GitHub release.

.EXAMPLE
  scripts\release.ps1 -Version 0.2.0              # bump, build, tag, push, publish
  scripts\release.ps1 -Version 0.2.0 -NoPublish   # only build dist\SnapCap-Setup-0.2.0.exe

.NOTES
  Add a "## [x.y.z] - date" section to CHANGELOG.md first: it becomes the release description.
  Needs: Rust, Inno Setup 6, GitHub CLI (gh, logged in). Run from a clean master checkout.
#>
param(
    [Parameter(Mandatory)][string]$Version,
    [switch]$NoPublish,
    # Optional "Co-Authored-By: ..." trailer line for the release commit.
    [string]$CommitTrailer = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root
$tag = "v$Version"

# Run a native command; fail the script if it exits non-zero (stderr noise alone isn't failure).
function Run {
    $eap = $ErrorActionPreference; $ErrorActionPreference = "Continue"
    & $args[0] $args[1..($args.Count - 1)]
    $code = $LASTEXITCODE; $ErrorActionPreference = $eap
    if ($code -ne 0) { throw "'$($args -join ' ')' failed with exit code $code" }
}

if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "Version must look like 1.2.3 (got '$Version')" }

# ---- preflight --------------------------------------------------------------------------
if (-not $NoPublish) {
    if ((git branch --show-current) -ne "master") { throw "Release from master (currently on '$(git branch --show-current)')." }
    if (git status --porcelain) { throw "Working tree isn't clean. Commit or stash first." }
    if (git tag -l $tag) { throw "Tag $tag already exists." }
    Run gh auth status
}

$changelog = Get-Content CHANGELOG.md -Raw
$m = [regex]::Match($changelog, "(?ms)^## \[$([regex]::Escape($Version))\][^\r\n]*\r?\n(.*?)(?=^## \[|\z)")
if (-not $m.Success) { throw "CHANGELOG.md has no '## [$Version]' section. Add release notes first." }
$notes = $m.Groups[1].Value.Trim()

$iscc = @(
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw "Inno Setup 6 not found. Install it from https://jrsoftware.org/isinfo.php" }

# ---- bump versions (root crate + hook crate) --------------------------------------------
# Remember exactly what these files contained, so a -NoPublish run can put them back as they
# were (a plain `git checkout` would also throw away any uncommitted edits you have in them).
$snapshot = @{}
foreach ($f in "Cargo.toml", "hook\Cargo.toml", "Cargo.lock") { $snapshot[$f] = [IO.File]::ReadAllBytes((Join-Path $root $f)) }
foreach ($f in "Cargo.toml", "hook\Cargo.toml") {
    $text = Get-Content $f -Raw
    $new = ([regex]'(?m)^version = "[^"]+"').Replace($text, "version = `"$Version`"", 1)
    if ($new -ne $text) { [IO.File]::WriteAllText((Join-Path $root $f), $new) }
}

# ---- build ------------------------------------------------------------------------------
Run cargo build --release          # builds snapcap.exe and snapcap_hook.dll; refreshes Cargo.lock
Run $iscc "/DAppVersion=$Version" /Q installer\snapcap.iss

$setup = Join-Path $root "dist\SnapCap-Setup-$Version.exe"
if (-not (Test-Path $setup)) { throw "Installer wasn't produced at $setup" }
$hash = (Get-FileHash $setup -Algorithm SHA256).Hash.ToLower()
$hashFile = "$setup.sha256"
"$hash  $(Split-Path $setup -Leaf)" | Set-Content $hashFile -Encoding ascii
Write-Host "Built $setup ($([math]::Round((Get-Item $setup).Length / 1MB, 1)) MB)"
Write-Host "SHA-256 $hash"

if ($NoPublish) {
    foreach ($f in $snapshot.Keys) { [IO.File]::WriteAllBytes((Join-Path $root $f), $snapshot[$f]) }   # undo the version bump, nothing else
    Write-Host "NoPublish: installer built, nothing committed, tagged or pushed."
    return
}

# ---- commit, tag, push, publish ---------------------------------------------------------
git add Cargo.toml hook/Cargo.toml Cargo.lock
git diff --cached --quiet
if ($LASTEXITCODE -ne 0) {
    $msg = "Release $tag"
    if ($CommitTrailer) { $msg += "`n`n$CommitTrailer" }
    $msgFile = New-TemporaryFile
    Set-Content $msgFile $msg -Encoding ascii
    Run git commit -q -F $msgFile
    Remove-Item $msgFile
}
Run git tag -a $tag -m "SnapCap $Version"
Run git push origin master
Run git push origin $tag

$notesFile = New-TemporaryFile
# UTF-8 *without* a BOM (Windows PowerShell's -Encoding utf8 adds one, which shows up as a stray
# character at the start of the GitHub release description).
[IO.File]::WriteAllText($notesFile.FullName, $notes, (New-Object System.Text.UTF8Encoding $false))
Run gh release create $tag $setup $hashFile --title "SnapCap $Version" --notes-file $notesFile --verify-tag
Remove-Item $notesFile
Write-Host "Published $tag : https://github.com/jinple/snapcap/releases/tag/$tag"
