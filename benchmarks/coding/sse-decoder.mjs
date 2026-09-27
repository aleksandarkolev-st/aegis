export class Decoder {
  constructor(maxBytes = 8192) {
    this.buffer = '';
    this.maxBytes = maxBytes;
    this.retry = undefined;
  }
  push(bytes) {
    this.buffer += new TextDecoder().decode(bytes);
    const records = this.buffer.split('\n\n');
    this.buffer = records.pop();
    return records.map(data => ({ event: 'message', data, id: '' }));
  }
  end() {
    this.buffer = '';
    return [];
  }
}
