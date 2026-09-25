import { mkdtempSync } from "node:fs";
import * as Os from "node:os";
import * as Path from "node:path";

process.env["PORCHLIGHT_HOME"] = mkdtempSync(Path.join(Os.tmpdir(), "porchlight-test-"));
