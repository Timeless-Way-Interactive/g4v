# Builds dist\g4v-setup-<version>.exe: the Windows installer, with g4v and its own Git.
#
# The setup program embeds g4v.exe. Git (MinGit, which includes Git Credential Manager) and Git LFS are appended to
# the end of the setup program as a zip, followed by a 16-byte trailer (zip length, then the magic "G4VGIT01"). The
# installer reads that zip from its own file; the uninstaller it leaves behind is a copy without it.
#
# The Git downloads are pinned in installer\git-bundle.json and refused unless their SHA-256 matches. They are cached
# in installer\target\bundle-cache. -NoGit builds an installer without Git (smaller, for testing).
param([switch]$NoGit)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$installer = Join-Path $root 'installer'

function Get-PinnedFile($pin, [string]$cache) {
    $file = Join-Path $cache ([IO.Path]::GetFileName($pin.url))
    if (-not (Test-Path $file)) {
        Write-Output "Downloading $($pin.url)"
        & curl.exe -sSfL -o "$file.part" $pin.url
        if ($LASTEXITCODE -ne 0) { throw "Download failed: $($pin.url)" }
        Move-Item "$file.part" $file -Force
    }
    $hash = (Get-FileHash $file -Algorithm SHA256).Hash.ToLower()
    if ($hash -ne $pin.sha256) {
        Remove-Item $file -Force
        throw "$file has SHA-256 $hash, expected $($pin.sha256). Deleted; check installer\git-bundle.json."
    }
    return $file
}

function New-GitBundle([string]$out) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $pins = Get-Content (Join-Path $installer 'git-bundle.json') -Raw | ConvertFrom-Json
    $cache = Join-Path $installer 'target\bundle-cache'
    New-Item -ItemType Directory -Force $cache | Out-Null
    $stage = Join-Path $installer 'target\git-bundle'
    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
    $git = Join-Path $stage 'git'

    [IO.Compression.ZipFile]::ExtractToDirectory((Get-PinnedFile $pins.mingit $cache), $git)
    $bin = Join-Path $git 'ucrt64\bin'
    if (-not (Test-Path (Join-Path $bin 'git-credential-manager.exe'))) { throw 'MinGit no longer contains Git Credential Manager; add it to git-bundle.json.' }

    $lfsTmp = Join-Path $stage 'lfs'
    [IO.Compression.ZipFile]::ExtractToDirectory((Get-PinnedFile $pins.git_lfs $cache), $lfsTmp)
    $lfs = Get-ChildItem $lfsTmp -Recurse -Filter 'git-lfs.exe' | Select-Object -First 1
    if (-not $lfs) { throw 'git-lfs.exe not found in the Git LFS archive.' }
    if (Test-Path (Join-Path $bin 'git-lfs.exe')) { throw 'MinGit now contains git-lfs.exe; decide which one to ship.' }
    Copy-Item $lfs.FullName $bin
    Remove-Item $lfsTmp -Recurse -Force

    # Git's system config for this bundle. MinGit's own includes any Git for Windows config on the machine, which
    # would make g4v behave differently from PC to PC; this one stands alone. Same defaults as Git for Windows.
    $config = @'
# g4v's bundled Git. Settings here apply only to the Git inside g4v; the user's own ~/.gitconfig still applies on top.
[core]
	symlinks = false
	autocrlf = true
	fscache = true
	# Unreal projects routinely exceed Windows' 260-character path limit.
	longpaths = true
[pack]
	packSizeLimit = 2g
[filter "lfs"]
	clean = git-lfs clean -- %f
	smudge = git-lfs smudge -- %f
	process = git-lfs filter-process
	required = true
[credential]
	helper = manager
[credential "https://dev.azure.com"]
	useHttpPath = true
'@
    [IO.File]::WriteAllText((Join-Path $git 'etc\gitconfig'), $config.Replace("`r`n", "`n"), (New-Object Text.UTF8Encoding $false))

    $notice = @"
g4v ships these programs unmodified. Each keeps its own license.

Git for Windows (MinGit) $($pins.mingit.version)
  License: $($pins.mingit.license)
  Source:  $($pins.mingit.source)
  Includes Git Credential Manager (MIT, https://github.com/git-ecosystem/git-credential-manager).

Git LFS $($pins.git_lfs.version)
  License: $($pins.git_lfs.license)
  Source:  $($pins.git_lfs.source)
"@
    [IO.File]::WriteAllText((Join-Path $git 'THIRD-PARTY-NOTICES.txt'), $notice)
    $marker = [ordered]@{ mingit = $pins.mingit.version; git_lfs = $pins.git_lfs.version } | ConvertTo-Json
    [IO.File]::WriteAllText((Join-Path $git '.g4v-bundle'), $marker)

    # Smoke test before shipping: the bundled git, LFS and GCM all start, and the config is ours.
    $g = Join-Path $git 'cmd\git.exe'
    $v = & $g --version; if ($LASTEXITCODE) { throw 'bundled git does not start' }
    $l = & $g lfs version; if ($LASTEXITCODE -or $l -notmatch [regex]::Escape($pins.git_lfs.version)) { throw "bundled git runs the wrong git-lfs: $l" }
    $c = & $g credential-manager --version; if ($LASTEXITCODE) { throw 'bundled Git Credential Manager does not start' }
    $origin = & $g config --system --show-origin --get core.longpaths
    if ($origin -notmatch [regex]::Escape($git.Replace('\', '/')) -and $origin -notmatch 'etc/gitconfig') { throw "unexpected system config: $origin" }
    Write-Output "Bundle: $v, $l, GCM $c"

    if (Test-Path $out) { Remove-Item $out -Force }
    [IO.Compression.ZipFile]::CreateFromDirectory($git, $out, [IO.Compression.CompressionLevel]::Optimal, $false)
}

Push-Location $root
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw 'cargo build (g4v) failed' }
    Push-Location $installer
    try {
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw 'cargo build (installer) failed' }
    } finally { Pop-Location }
    $version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
    $dist = Join-Path $root 'dist'
    New-Item -ItemType Directory -Force $dist | Out-Null
    $out = Join-Path $dist "g4v-setup-$version.exe"
    Copy-Item (Join-Path $installer 'target\release\g4v-setup.exe') $out -Force
    if (-not $NoGit) {
        $zip = Join-Path $installer 'target\git-bundle.zip'
        New-GitBundle $zip
        $len = (Get-Item $zip).Length
        $fs = [IO.File]::Open($out, 'Append', 'Write')
        try {
            $in = [IO.File]::OpenRead($zip)
            try { $in.CopyTo($fs) } finally { $in.Dispose() }
            $fs.Write([BitConverter]::GetBytes([uint64]$len), 0, 8)
            $magic = [Text.Encoding]::ASCII.GetBytes('G4VGIT01')
            $fs.Write($magic, 0, 8)
        } finally { $fs.Dispose() }
    }
    Write-Output ("Installer: {0} ({1:N0} MB)" -f $out, ((Get-Item $out).Length / 1MB))
} finally { Pop-Location }
