# fetch-incidents.ps1 — pull incident JSON files out of the GCS bucket
# into a local directory for offline inspection.
#
# Usage:
#   .\tools\fetch-incidents.ps1                    # last 24 hours
#   .\tools\fetch-incidents.ps1 -Days 7            # last 7 days
#   .\tools\fetch-incidents.ps1 -Kind score_mismatch
#
# Each incident is a small JSON blob; the bucket has a 90-day lifecycle
# so this is also the way to grab anything before it auto-deletes.

param(
    [int]$Days = 1,
    [string]$Kind = "",
    [string]$Project = "quarterframe",
    [string]$Bucket = "quarterframe-freeplay-incidents",
    [string]$OutDir = ".incidents"
)

if (-not (Get-Command gcloud -ErrorAction SilentlyContinue)) {
    Write-Error "gcloud not on PATH. Install Google Cloud SDK."
    exit 1
}

$null = New-Item -ItemType Directory -Force -Path $OutDir

# gsutil's date filtering is awkward. Just rsync each day-partition we
# want and let the local filter step handle finer-grained selection.
$today = Get-Date
for ($i = 0; $i -lt $Days; $i++) {
    $d = $today.AddDays(-$i)
    $prefix = "{0:yyyy}/{0:MM}/{0:dd}" -f $d
    $local = Join-Path $OutDir $prefix
    $null = New-Item -ItemType Directory -Force -Path $local

    Write-Host "Fetching gs://$Bucket/$prefix/ ..."
    & gcloud storage rsync "gs://$Bucket/$prefix/" $local --project=$Project 2>&1 | Out-Null
}

Write-Host ""
Write-Host "Incidents downloaded to: $OutDir"
Write-Host ""

# Summarize by kind so you can see at a glance what happened.
$files = Get-ChildItem -Path $OutDir -Filter "*.json" -Recurse
if (-not $files) {
    Write-Host "No incidents in the requested window."
    return
}

$summary = @{}
foreach ($f in $files) {
    try {
        $j = Get-Content $f.FullName -Raw | ConvertFrom-Json
        $payload_kind = $j.payload.kind
        if ($Kind -and $payload_kind -ne $Kind) { continue }
        if (-not $summary.ContainsKey($payload_kind)) {
            $summary[$payload_kind] = 0
        }
        $summary[$payload_kind]++
    } catch {
        Write-Warning "Failed to parse $($f.Name): $_"
    }
}

Write-Host "Counts by kind:"
$summary.GetEnumerator() | Sort-Object Value -Descending | ForEach-Object {
    "  {0,-30} {1}" -f $_.Key, $_.Value
}
Write-Host ""
Write-Host "Latest 10 (most recent first):"
$files | Sort-Object LastWriteTime -Descending | Select-Object -First 10 | ForEach-Object {
    try {
        $j = Get-Content $_.FullName -Raw | ConvertFrom-Json
        if ($Kind -and $j.payload.kind -ne $Kind) { return }
        "  {0}  {1,-30}  {2}" -f $j.recorded_at, $j.payload.kind, ($j.payload.summary -replace '\s+', ' ').Substring(0, [Math]::Min(80, ($j.payload.summary -replace '\s+', ' ').Length))
    } catch { }
}
