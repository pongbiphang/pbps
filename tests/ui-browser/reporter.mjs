import { stripVTControlCharacters } from 'node:util';
// Browser traces and stock error attachments can disclose fragment credentials.
// Emit bounded diagnostics only; the runner keeps this output outside the tree.
export function redact(value) {
  return stripVTControlCharacters(String(value)).replace(/postgres(?:ql)?:\/\/[^\s"']+/gi, '[database]')
    .replace(/[a-f0-9]{64}/gi, '[digest/token]')
    .replace(/(password|x-pbps-token)\s*[:=]\s*\S+/gi, '$1=[redacted]');
}
export default class Reporter {
  onBegin(_, suite) { this.total = suite.allTests().length; this.executed = 0; this.passed = 0; }
  onTestEnd(test, result) {
    this.executed++; if (result.status === "passed") this.passed++;
    console.log(JSON.stringify({test: test.title, status: result.status,
      errors: result.errors.map(e => redact(e.message).slice(0, 3000))}));
  }
  onStdOut(chunk) { process.stdout.write(redact(chunk)); }
  onStdErr(chunk) { process.stderr.write(redact(chunk)); }
  onError(error) { console.error(redact(error.message).slice(0, 3000)); }
  onEnd(result) {
    console.log(JSON.stringify({suite: 'shipped-ui-browser', total: this.total,
      executed: this.executed, passed: this.passed, status: result.status}));
    if (!this.total || this.executed !== this.total || this.passed !== this.total) return { status: 'failed' };
  }
}
