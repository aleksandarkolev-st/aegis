export class Decoder {
  constructor(maxBytes = 8192) {
    if (!Number.isSafeInteger(maxBytes) || maxBytes <= 0) throw new Error('Invalid byte bound');
    this.maxBytes = maxBytes;
    this.pendingBytes = 0;
    this.line = [];
    this.skipLF = false;
    this.firstLine = true;
    this.data = [];
    this.event = '';
    this.id = '';
    this.retry = undefined;
  }
  push(bytes) {
    if (!(bytes instanceof Uint8Array)) throw new Error('Expected bytes');
    const events = [];
    for (const byte of bytes) {
      if (this.skipLF) {
        this.skipLF = false;
        if (byte === 10) continue;
      }
      this.pendingBytes += 1;
      if (this.pendingBytes > this.maxBytes) throw new Error('Pending event exceeds byte bound');
      if (byte === 10 || byte === 13) {
        this.completeLine(events);
        this.skipLF = byte === 13;
      } else this.line.push(byte);
    }
    return events;
  }
  completeLine(events) {
    let line = new TextDecoder('utf-8', { ignoreBOM: true }).decode(Uint8Array.from(this.line));
    this.line = [];
    if (this.firstLine) {
      this.firstLine = false;
      if (line.startsWith('\uFEFF')) line = line.slice(1);
    }
    if (!line) {
      if (this.data.length) events.push({ event: this.event || 'message', id: this.id, data: this.data.join('\n') });
      this.data = [];
      this.event = '';
      this.pendingBytes = 0;
      return;
    }
    if (line.startsWith(':')) return;
    const separator = line.indexOf(':');
    const field = separator < 0 ? line : line.slice(0, separator);
    let value = separator < 0 ? '' : line.slice(separator + 1);
    if (value.startsWith(' ')) value = value.slice(1);
    if (field === 'data') this.data.push(value);
    else if (field === 'event') this.event = value;
    else if (field === 'id' && !value.includes('\u0000')) this.id = value;
    else if (field === 'retry' && /^\d+$/.test(value) && Number.isFinite(Number(value))) this.retry = Number(value);
  }
  end() {
    this.line = [];
    this.data = [];
    this.event = '';
    this.pendingBytes = 0;
    this.skipLF = false;
    return [];
  }
}
