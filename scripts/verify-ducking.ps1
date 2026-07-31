
# End-to-end verification of the audio ducker. Entirely silent: victims open real
# WASAPI render streams but never queue any audio.
#
# Takes roughly 4-5 minutes and needs a real active render endpoint - on a machine
# with none (RDP, CI) every scenario reports FAIL rather than erroring.
#
# It only ever resets volumes for its own executables under target/release/examples,
# never for the applications the user is actually running.
param(
  [string]$Exe = (Join-Path $PSScriptRoot "..\target\release\examples\duck_lab.exe"),
  [double]$Ratio = 0.95
)
$ErrorActionPreference = 'Stop'
$script:pass = 0; $script:fail = 0

# Turn OFF the one-time repair sweep for these runs. It would raise any quiet
# session to 1.0 on its own and could mask a failure of the mechanism actually
# under test (baseline capture and heal-on-reappear). Child processes inherit this.
$env:SPEAKSTREAM_NO_MIGRATION = '1'

function VictimVol {
  # state 2 == AudioSessionStateExpired: a stream that has already ended and is
  # not audible. Only live streams represent what the user would actually hear.
  # Take the minimum so any live stream left too quiet fails the check.
  $rows = (& $Exe dump | Out-String | ConvertFrom-Json)
  $v = @($rows | Where-Object { $_.id -like '*duck_lab.exe*' -and $_.state -ne 2 })
  if ($v.Count -gt 0) { return [double](($v | Measure-Object -Property volume -Minimum).Minimum) }
  return $null
}

function Check([string]$name, $actual, [double]$expected) {
  if ($null -ne $actual -and [Math]::Abs($actual - $expected) -lt 0.01) {
    Write-Output ("  PASS  {0}  (got {1})" -f $name, $actual); $script:pass++
  } else {
    Write-Output ("  FAIL  {0}  (expected {1}, got {2})" -f $name, $expected, $actual); $script:fail++
  }
}

function CleanSlate {
  Get-Process duck_lab, other_app -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 500
  $p = if ($env:SPEAKSTREAM_STATE_DIR) {
         Join-Path $env:SPEAKSTREAM_STATE_DIR 'duck-state-v2.txt'
       } else {
         Join-Path $env:LOCALAPPDATA 'speakstream\duck-state-v2.txt'
       }
  Remove-Item $p -Force -ErrorAction SilentlyContinue
  # The audio engine remembers a volume per application, so it can only be reset
  # while that application actually has a live session. Open one, and WAIT until
  # it is really visible - a fixed sleep sometimes missed it, which silently
  # started a scenario from a stale remembered volume.
  $r = Start-Process $Exe -ArgumentList 'victim','hold','6' -PassThru -WindowStyle Hidden
  $deadline = (Get-Date).AddSeconds(10)
  do {
    Start-Sleep -Milliseconds 300
    $seen = @((& $Exe dump | Out-String | ConvertFrom-Json) |
              Where-Object { $_.id -like '*duck_lab.exe*' -and $_.state -ne 2 }).Count
  } while ($seen -eq 0 -and (Get-Date) -lt $deadline)
  # Filtered to this harness's own executables. An unfiltered reset would force
  # every application on the machine to 100% and destroy the user's real
  # per-application volume settings.
  & $Exe set-all 1.0 'examples' | Out-Null
  Start-Sleep -Milliseconds 400
  if ((VictimVol) -ne 1.0) { Write-Output "  (warning: clean slate did not reach 1.0)" }
  $r.WaitForExit()
  Remove-Item $p -Force -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 300
}

Write-Output "############ SCENARIO 1: plain duck/restore, victim alive throughout ############"
CleanSlate
$v = Start-Process $Exe -ArgumentList 'victim','hold','16' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
Check 'victim starts at full volume' (VictimVol) 1.0
$d = Start-Process $Exe -ArgumentList 'duck',"$Ratio",'5','0' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 3
Check 'victim is ducked while speaking' (VictimVol) $Ratio
$d.WaitForExit(); Start-Sleep -Seconds 2
Check 'victim restored after speech' (VictimVol) 1.0
$v.WaitForExit()

Write-Output ""
Write-Output "############ SCENARIO 2: victim's session DIES mid-duck, returns later ############"
Write-Output "############   (this is the desk-talk beep case -- the original bug)   ############"
CleanSlate
$v1 = Start-Process $Exe -ArgumentList 'victim','hold','8' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
Check 'V1 starts at full volume' (VictimVol) 1.0
# ducker holds 10s then lingers 20s, like an always-running tray app
$d = Start-Process $Exe -ArgumentList 'duck',"$Ratio",'10','20' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
Check 'V1 ducked' (VictimVol) $Ratio
$v1.WaitForExit()
Write-Output "  (V1 gone; ducker still speaking)"
Start-Sleep -Seconds 10   # duck window ends around here
$v2 = Start-Process $Exe -ArgumentList 'victim','hold','12' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 4
Check 'V2 REPAIRED after reappearing' (VictimVol) 1.0
$d.WaitForExit(); $v2.WaitForExit()

Write-Output ""
Write-Output "############ SCENARIO 3: ratchet test - 4 cycles, session dies every time ############"
Write-Output "############   a resident tool stays running throughout, as in real use  ############"
CleanSlate
# Models the always-running tray tools (speak-selected, quick-assistant): a
# guardian is alive the whole time and repairs sessions as they reappear.
$resident = Start-Process $Exe -ArgumentList 'heal','80' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 1
$seq = @()
for ($i = 1; $i -le 4; $i++) {
  $v = Start-Process $Exe -ArgumentList 'victim','hold','4' -PassThru -WindowStyle Hidden
  Start-Sleep -Seconds 2
  $seq += (VictimVol)
  $d = Start-Process $Exe -ArgumentList 'duck',"$Ratio",'8','1' -PassThru -WindowStyle Hidden
  $v.WaitForExit(); $d.WaitForExit()
  Start-Sleep -Seconds 2
}
$v = Start-Process $Exe -ArgumentList 'victim','hold','8' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 4
$final = VictimVol
Write-Output ("  volume observed at the start of each cycle: {0}" -f ($seq -join ' -> '))
Check 'no ratcheting after 4 cycles' $final 1.0
$v.WaitForExit()
Stop-Process -Id $resident.Id -Force -ErrorAction SilentlyContinue

Write-Output ""
Write-Output "############ SCENARIO 4: ducker CRASHES while ducked; another tool heals ############"
CleanSlate
$v = Start-Process $Exe -ArgumentList 'victim','hold','30' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
$d = Start-Process $Exe -ArgumentList 'duck-crash',"$Ratio",'3' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
Check 'victim ducked before the crash' (VictimVol) $Ratio
$d.WaitForExit()
Write-Output "  (ducker died without restoring)"
Start-Sleep -Seconds 2
$h = Start-Process $Exe -ArgumentList 'heal','15' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 12
Check 'another tool healed the crash damage' (VictimVol) 1.0
$h.WaitForExit(); $v.WaitForExit()

Write-Output ""
Write-Output "############ SCENARIO 5: two duckers overlap; last one out restores ############"
CleanSlate
$v = Start-Process $Exe -ArgumentList 'victim','hold','26' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
$d1 = Start-Process $Exe -ArgumentList 'duck',"$Ratio",'6','2' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
$d2 = Start-Process $Exe -ArgumentList 'duck',"$Ratio",'12','2' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
Check 'ducked once, not twice (no compounding)' (VictimVol) $Ratio
$d1.WaitForExit()
Start-Sleep -Seconds 2
Check 'still ducked while the 2nd ducker speaks' (VictimVol) $Ratio
$d2.WaitForExit()
Start-Sleep -Seconds 4
Check 'restored once both finished' (VictimVol) 1.0
$v.WaitForExit()

Write-Output ""
Write-Output "############ SCENARIO 6: desk-talk beep pattern - stream opens/closes rapidly ############"
CleanSlate
$resident = Start-Process $Exe -ArgumentList 'heal','60' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 1
# 300ms of stream, 500ms of silence, repeatedly: exactly how desk-talk's
# per-beep DefaultDeviceSink behaves.
$pulser = Start-Process $Exe -ArgumentList 'victim','pulse','45','300','500' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 3
$d = Start-Process $Exe -ArgumentList 'duck',"$Ratio",'10','3' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 6
$duringSamples = @()
for ($i = 0; $i -lt 12; $i++) { $s = VictimVol; if ($null -ne $s) { $duringSamples += $s }; Start-Sleep -Milliseconds 250 }
$d.WaitForExit()
Start-Sleep -Seconds 4
$afterSamples = @()
for ($i = 0; $i -lt 25; $i++) { $s = VictimVol; if ($null -ne $s) { $afterSamples += $s }; Start-Sleep -Milliseconds 400 }
$pulser.WaitForExit()
Stop-Process -Id $resident.Id -Force -ErrorAction SilentlyContinue
Write-Output ("  samples while speaking : {0}" -f (($duringSamples | Select-Object -Unique) -join ', '))
Write-Output ("  samples after speaking : {0}" -f (($afterSamples | Select-Object -Unique) -join ', '))
$worst = ($afterSamples | Measure-Object -Minimum).Minimum
if ($afterSamples.Count -eq 0) {
  Write-Output "  FAIL  no samples captured after speaking"; $script:fail++
} else {
  Check 'every beep back at full volume after speaking' $worst 1.0
}

Write-Output ""
Write-Output "############ SCENARIO 7: a speaking tool must not have ITS OWN voice ducked ############"
Write-Output "############   (two tools installed, each running a guardian)          ############"
CleanSlate
# The victim must be a DIFFERENT executable from the speaker, otherwise they
# share one session identifier and the two roles are indistinguishable -- just
# as speak-selected.exe and desk-talk.exe are different binaries in real use.
$otherExe = Join-Path (Split-Path $Exe) 'other_app.exe'
Copy-Item $Exe $otherExe -Force
# models quick-assistant sitting idle in the tray with a guardian running
$resident = Start-Process $Exe -ArgumentList 'heal','40' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 1
$victim = Start-Process $otherExe -ArgumentList 'victim','hold','30' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
# models speak-selected: owns a voice session AND ducks everything else
$speaker = Start-Process $Exe -ArgumentList 'speak',"$Ratio",'12','3' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 5
$rows = (& $Exe dump | Out-String | ConvertFrom-Json)
$speakerVol = @($rows | Where-Object { $_.pid -eq $speaker.Id -and $_.state -ne 2 } |
                Measure-Object -Property volume -Minimum).Minimum
$victimVol  = @($rows | Where-Object { $_.pid -eq $victim.Id -and $_.state -ne 2 } |
                Measure-Object -Property volume -Minimum).Minimum
Write-Output ("  speaker pid={0} vol={1} | victim pid={2} vol={3}" -f $speaker.Id, $speakerVol, $victim.Id, $victimVol)
Check "the speaker's own voice stays at full volume" $speakerVol 1.0
Check 'the other app is ducked'                      $victimVol  $Ratio
$speaker.WaitForExit()
Start-Sleep -Seconds 5
$after = @((& $Exe dump | Out-String | ConvertFrom-Json) |
           Where-Object { $_.pid -eq $victim.Id -and $_.state -ne 2 } |
           Measure-Object -Property volume -Minimum).Minimum
Check 'other app restored once speech ended' $after 1.0
$victim.WaitForExit()
Stop-Process -Id $resident.Id -Force -ErrorAction SilentlyContinue
Remove-Item $otherExe -Force -ErrorAction SilentlyContinue

Write-Output ""
Write-Output "=================================================="
Write-Output ("RESULT: {0} passed, {1} failed" -f $script:pass, $script:fail)
Write-Output "=================================================="
CleanSlate
& $Exe set-all 1.0 'examples'
