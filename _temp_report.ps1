$files = Get-ChildItem -Recurse -Filter *.rs src/
$results = @()
foreach ($f in $files) {
    $lines = (Get-Content $f.FullName | Measure-Object -Line).Lines
    $uw = (Select-String -LiteralPath $f.FullName -Pattern 'unwrap(' -SimpleMatch).Count
    $us = (Select-String -LiteralPath $f.FullName -Pattern 'unsafe{' -SimpleMatch).Count
    $obj = New-Object PSObject -Property @{File=$f.FullName; Lines=$lines; unwrap_Calls=$uw; unsafe_Blocks=$us}
    $results += $obj
}
$results | Sort-Object Lines -Descending | Format-Table -AutoSize | Out-String -Width 200