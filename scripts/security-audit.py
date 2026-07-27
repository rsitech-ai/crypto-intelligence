#!/usr/bin/env python3
import hashlib,json,re,sys
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]; findings=[]
def add(severity,path,issue):findings.append({"severity":severity,"path":path,"issue":issue})
pem_private_key=re.compile(
 b"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----\\r?\\n"
 b"[A-Za-z0-9+/=\\r\\n]{32,}\\r?\\n-----END "
)
for p in ROOT.rglob("*"):
 if not p.is_file() or ".git" in p.parts or ".build" in p.parts or "__pycache__" in p.parts:continue
 rel=str(p.relative_to(ROOT)); data=p.read_bytes()
 if pem_private_key.search(data):add("critical",rel,"private key material")
 if len(data)>64*1024*1024:add("medium",rel,"unexpected oversized repository file")
 if p.suffix in{".rs",".swift",".py",".sh",".toml",".json",".yml",".yaml"}:
  text=data.decode("utf-8","replace")
  if re.search(r"(?i)(?<![\"'])\b(password|api[_-]?secret|private[_-]?key)\b\s*=\s*[\"'][^\"']+[\"']",text):add("high",rel,"hard-coded secret-shaped value")
for required in["SECURITY.md","docs/security/threat-model.md","crates/security-policy/src/lib.rs","crates/recovery/src/lib.rs","crates/artifact-signing/src/lib.rs","crates/shadow-ledger/src/lib.rs"]:
 if not (ROOT/required).is_file():add("high",required,"required security boundary missing")
report={"schema_version":1,"repository":"s1korrrr/crypto-inteligence","scope":"secret-pattern and required-boundary-presence scan","limitations":["does not verify security implementation or runtime behavior"],"findings":findings,"counts":{s:sum(f["severity"]==s for f in findings)for s in["critical","high","medium","low"]},"status":"passed" if not any(f["severity"] in{"critical","high"}for f in findings)else"failed"}
out=ROOT/"release/evidence/security-audit.json";out.parent.mkdir(parents=True,exist_ok=True);out.write_text(json.dumps(report,indent=2)+"\n")
print(json.dumps(report["counts"]));raise SystemExit(0 if report["status"]=="passed" else 1)
