# Installs the pinned LunarG Vulkan SDK on a Windows runner (headers, loader library, glslc) for the Vulkan
# Whisper helper's build, after checking the installer against the checksum LunarG publishes.
$ErrorActionPreference = 'Stop'

$version = '1.4.363.1'
$sha256 = 'c02932bd9a968b22d404536108adaacac908c081cd6b0113f5bfefc523dd0a6a'
$installer = Join-Path $env:RUNNER_TEMP "vulkansdk-windows-X64-$version.exe"

Invoke-WebRequest -Uri "https://sdk.lunarg.com/sdk/download/$version/windows/vulkansdk-windows-X64-$version.exe" -OutFile $installer
$actual = (Get-FileHash -Algorithm SHA256 $installer).Hash.ToLowerInvariant()
if ($actual -ne $sha256) { throw "Vulkan SDK installer checksum $actual, expected $sha256" }

& $installer --accept-licenses --default-answer --confirm-command install | Out-Host
if ($LASTEXITCODE -ne 0) { throw "Vulkan SDK installer exited with $LASTEXITCODE" }

$sdk = "C:\VulkanSDK\$version"
if (-not (Test-Path "$sdk\Bin\glslc.exe")) { throw "glslc.exe missing under $sdk" }
Add-Content $env:GITHUB_ENV "VULKAN_SDK=$sdk"
Add-Content $env:GITHUB_PATH "$sdk\Bin"
