#!/usr/bin/env python3
from __future__ import annotations
import json,re,subprocess,sys,tomllib
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]; errors=[]; warnings=[]
def fail(x): errors.append(x)
def ignored_generated_path(path:Path):
 return any(part in{".git",".build","target","__pycache__"} for part in path.parts)
def placeholder_reason(path:Path,text:str):
 if text.strip()==path.as_posix():
  return "path-only source placeholder"
 if path.suffix==".rs" and "/tests/" in f"/{path.as_posix()}" and re.search(r"assert_eq!\(\s*1(?:_u32)?\s*,\s*1(?:_u32)?\s*\)",text):
  return "tautological planned-contract test"
 meaningful=[line.strip() for line in text.splitlines() if line.strip()]
 if meaningful and all(line.startswith("//") for line in meaningful) and "implementation boundary" in text:
  return "comment-only implementation boundary"
 if path.suffix==".rs" and "implementation boundary" in text and re.search(r"pub struct \w+Contract\b",text) and "schema_version" in text and "identifier" in text:
  return "generic identifier contract module"
 compact=re.sub(r"\s+","",text)
 if path.suffix==".swift" and re.fullmatch(r"importFoundationpublicenum\w+Contract:Sendable\{publicstaticletschemaVersion:UInt32=1\}",compact):
  return "contract-only Swift source"
 if path.suffix==".sh" and len(meaningful)<=3 and any("installed" in line for line in meaningful):
  return "installed-only shell placeholder"
 return None
def cargo_manifest_reason(path:Path):
 try:data=tomllib.loads(path.read_text(encoding="utf-8"))
 except Exception:return None
 if "package" not in data and "workspace" not in data:
  return "missing [package] or [workspace] table"
 return None
def xcode_project_reason(path:Path):
 try:text=path.read_text(encoding="utf-8")
 except Exception:return "unreadable Xcode project"
 if not text.startswith("// !$*UTF8*$!") or "isa = PBXProject;" not in text:
  return "missing Xcode project structure"
 return None
def workflow_reason(path:Path):
 try:text=path.read_text(encoding="utf-8")
 except Exception:return "unreadable workflow"
 if not re.search(r"(?m)^on\s*:",text) or not re.search(r"(?m)^jobs\s*:",text):
  return "missing workflow triggers or jobs"
 return None
def contains_hard_coded_secret(text:str):
 pem=re.compile(r"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----\r?\n[A-Za-z0-9+/=\r\n]{32,}\r?\n-----END ")
 assignment=re.compile(r"(?i)(?<![\"'])\b(?:api[_-]?secret|private[_-]?key|password)\b\s*=\s*[\"'][^\"']+[\"']")
 return bool(pem.search(text) or assignment.search(text))
def read(p):
 q=ROOT/p
 if not q.is_file(): fail(f"missing {p}"); return ""
 return q.read_text(encoding="utf-8")
def expected():
 p=ROOT/"docs/implementation/expected-paths.json"
 try:return json.loads(p.read_text())
 except Exception as e: fail(f"expected paths: {e}"); return {}
def plan():
 m=expected(); missing=sorted(p for p in m if not (ROOT/p).exists())
 if len(m)!=662: fail(f"expected 662 planned paths, found {len(m)}")
 for p in missing: fail(f"planned path missing: {p}")
 report={"schema_version":1,"expected_path_count":len(m),"missing":missing,"paths":{p:{"owners":o,"exists":(ROOT/p).exists()}for p,o in sorted(m.items())}}
 (ROOT/"docs/implementation/PLAN-TRACEABILITY.json").write_text(json.dumps(report,indent=2)+"\n")
def parsers():
 for p in ROOT.rglob("*.toml"):
  if ignored_generated_path(p): continue
  try:tomllib.loads(p.read_text())
  except Exception as e:fail(f"TOML {p.relative_to(ROOT)}: {e}")
  if p.name=="Cargo.toml":
   reason=cargo_manifest_reason(p)
   if reason:fail(f"Cargo manifest {p.relative_to(ROOT)}: {reason}")
 for p in ROOT.rglob("*.json"):
  if ignored_generated_path(p):continue
  try:json.loads(p.read_text())
  except Exception as e:fail(f"JSON {p.relative_to(ROOT)}: {e}")
 project=ROOT/"apps/macos/CuspObservatory.xcodeproj/project.pbxproj"
 if project.is_file():
  reason=xcode_project_reason(project)
  if reason:fail(f"Xcode project {project.relative_to(ROOT)}: {reason}")
def authority():
 fixed=read("crates/fixed-decimal/src/lib.rs")
 parser=fixed[fixed.find("pub fn parse"):fixed.find("pub fn rescale_exact")]
 if "f32" in parser or "f64" in parser:fail("authoritative decimal parser uses float")
 for x in["checked_add","checked_sub","checked_mul","checked_div_exact","to_f64_lossy_for_analysis","i128::MIN"]:
  if x not in fixed:fail(f"fixed decimal missing {x}")
 book="\n".join(read(p) for p in[
  "crates/orderbook/src/lib.rs",
  "crates/orderbook/src/book.rs",
  "crates/orderbook/src/checksum.rs",
  "crates/orderbook/src/state.rs",
 ])
 for x in["BTreeMap<Price, LevelValue>","GapDetected","ChecksumMismatch","StaleInstrumentGeneration"]:
  if x not in book:fail(f"orderbook missing {x}")
 data="\n".join([
  read("crates/dataset/src/lib.rs"),
  read("crates/dataset/src/folds.rs"),
 ]).replace(" ","").replace("\n","")
 for x in["feature.as_known_at_ns>origin_time_ns","feature.event_time_end_ns>origin_time_ns","purge_embargo_ns"]:
  if x not in data:fail(f"dataset missing {x}")
 event=read("crates/event-envelope/src/lib.rs")
 for x in["cmti:event:v1","raw_payload_hash","receive_monotonic_ns","connection_epoch","quality_score_ppm","IdentityMismatch"]:
  if x not in event:fail(f"event missing {x}")
def security():
 sec=read("crates/security-policy/src/lib.rs")
 for x in["OsRng","ConstantTimeEq","read_secret_fd","NetworkPurpose","OutboundDenied"]:
  if x not in sec:fail(f"security missing {x}")
 rec=read("crates/recovery/src/lib.rs")
 for x in["XChaCha20Poly1305","OsRng.fill_bytes","associated_data_hash","symlink","sync_all"]:
  if x not in rec:fail(f"recovery missing {x}")
 obs=read("crates/observability/src/lib.rs")
 for x in["buffered_lines_limit","lossy(true)","DroppedLogLines","contains_json_secret","REDACTED"]:
  if x not in obs:fail(f"observability missing {x}")
 manifests="\n".join(p.read_text().lower() for p in ROOT.rglob("Cargo.toml") if not ignored_generated_path(p))
 for x in["opentelemetry","sentry"]:
  if x in manifests:fail(f"remote telemetry dependency {x}")
def governance():
 shadow=read("crates/shadow-ledger/src/lib.rs")
 for x in["ShadowPayload","sequence","previous_hash","UnknownForecast" if "UnknownForecast" in shadow else "Ordering","outcome_as_known_at_ns"]:
  if x not in shadow:fail(f"shadow missing {x}")
 promotion=read("crates/promotion-gate/src/lib.rs").replace(" ","")
 for x in["minimum_walkforward_positives:50","minimum_shadow_positives:20","minimum_regimes:3","minimum_positive_fold_fraction:0.70","security_review_id"]:
  if x not in promotion:fail(f"promotion missing {x}")
 signing=read("crates/artifact-signing/src/lib.rs")
 for x in["KeyRole","revoked_at_ns","created_at_ns","payload_blake3","key_id"]:
  if x not in signing:fail(f"signing missing {x}")
 release=read("crates/release/src/lib.rs")
 for x in["symlink","Component::Normal","sha256","stable_eligible","GateStatus"]:
  if x not in release:fail(f"release missing {x}")
def protobuf():
 protos=sorted((ROOT/"proto/cmti").rglob("*.proto"))
 if len(protos)!=7:fail(f"expected 7 protobuf files, found {len(protos)}")
 for p in protos:
  t=p.read_text()
  if 'syntax = "proto3";' not in t:fail(f"not proto3 {p}")
  if re.search(r"\b(float|double)\b",t):fail(f"float in {p}")
  for body in re.findall(r"\benum\s+\w+\s*\{(.*?)\}",t,re.S):
   values=[l.strip() for l in body.splitlines() if "=" in l and l.strip().endswith(";")]
   if not values or "UNSPECIFIED" not in values[0] or not re.search(r"=\s*0\s*;",values[0]):fail(f"enum zero in {p}")
def scripts():
 for p in sorted((ROOT/"scripts").glob("*.sh")):
  r=subprocess.run(["bash","-n",str(p)],capture_output=True,text=True)
  if r.returncode:fail(f"shell {p.name}: {r.stdout}{r.stderr}")
 for p in sorted((ROOT/".github/workflows").glob("*.yml")):
  reason=workflow_reason(p)
  if reason:fail(f"workflow {p.relative_to(ROOT)}: {reason}")
  for n,l in enumerate(p.read_text().splitlines(),1):
   m=re.search(r"\buses:\s*([^\s#]+)",l)
   if m and not m.group(1).startswith("./") and not re.fullmatch(r".+@[0-9a-f]{40}",m.group(1)):fail(f"unpinned action {p}:{n}")
def hygiene():
 pat=re.compile(r"\b(TBD|FIXME|TODO)\b|todo!\s*\(|unimplemented!\s*\(")
 for base in["crates","apps","scripts","proto","config","configs"]:
  d=ROOT/base
  if not d.exists():continue
  for p in d.rglob("*"):
   if not p.is_file() or p.resolve()==Path(__file__).resolve() or ignored_generated_path(p) or p.suffix not in{".rs",".swift",".py",".sh",".proto",".toml",".json",".yaml",".yml"}:continue
   t=p.read_text(errors="replace")
   if pat.search(t):fail(f"placeholder {p.relative_to(ROOT)}")
   reason=placeholder_reason(p.relative_to(ROOT),t)
   if reason:fail(f"placeholder {p.relative_to(ROOT)}: {reason}")
   if contains_hard_coded_secret(t):fail(f"secret-shaped content {p.relative_to(ROOT)}")
 for p in ROOT.rglob("*"):
  if ignored_generated_path(p):continue
  try:
   if p.is_symlink():fail(f"symlink {p.relative_to(ROOT)}")
  except OSError:fail(f"unreadable {p}")
def swift():
 pkg=read("apps/macos/Package.swift")
 if "swiftLanguageModes:[.v6]" not in pkg.replace(" ",""):fail("Swift 6 mode missing")
 allswift="\n".join(p.read_text() for p in (ROOT/"apps/macos/PortableSources").rglob("*.swift"))
 for x in["ProbabilityPPM","TransitionRPCClient","CircuitBreaker","EvidenceRenderer","ObservatoryStore"]:
  if x not in allswift:fail(f"Swift missing {x}")
def main():
 plan();parsers();authority();security();governance();protobuf();scripts();hygiene();swift()
 for w in warnings:print("warning:",w,file=sys.stderr)
 if errors:
  for e in errors:print("error:",e,file=sys.stderr)
  print(f"static audit failed with {len(errors)} error(s)",file=sys.stderr);return 1
 print(f"static audit: PASS ({len(expected())} planned paths, {len(warnings)} warnings)");return 0
if __name__=="__main__":raise SystemExit(main())
