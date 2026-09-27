import { isDeepStrictEqual } from 'node:util';

export function applyPatch(document, operations) {
  let result = structuredClone(document);
  const pointer = path => {
    if (typeof path !== 'string' || path && !path.startsWith('/')) throw new Error('Invalid pointer');
    if (!path) return [];
    return path.slice(1).split('/').map(component => {
      if (/~(?:[^01]|$)/.test(component)) throw new Error('Invalid escape');
      const decoded = component.replace(/~1/g, '/').replace(/~0/g, '~');
      if (['__proto__', 'constructor', 'prototype'].includes(decoded)) throw new Error('Unsafe pointer');
      return decoded;
    });
  };
  const key = (parent, component, insertion) => {
    if (parent === null || typeof parent !== 'object') throw new Error('Non-container parent');
    if (!Array.isArray(parent)) return component;
    if (component === '-' && insertion) return parent.length;
    if (!/^(0|[1-9]\d*)$/.test(component)) throw new Error('Invalid array index');
    const index = Number(component);
    if (!Number.isSafeInteger(index) || index < 0 || index >= parent.length + Number(insertion)) throw new Error('Out of bounds');
    return index;
  };
  const parentAt = parts => {
    let parent = result;
    for (const component of parts.slice(0, -1)) {
      const property = key(parent, component, false);
      if (!Object.hasOwn(parent, property)) throw new Error('Missing parent');
      parent = parent[property];
    }
    return parent;
  };
  const read = parts => {
    if (!parts.length) return result;
    const parent = parentAt(parts);
    const property = key(parent, parts.at(-1), false);
    if (!Object.hasOwn(parent, property)) throw new Error('Missing target');
    return parent[property];
  };
  const remove = parts => {
    if (!parts.length) { result = null; return; }
    const parent = parentAt(parts);
    const property = key(parent, parts.at(-1), false);
    if (!Object.hasOwn(parent, property)) throw new Error('Missing target');
    if (Array.isArray(parent)) parent.splice(property, 1);
    else delete parent[property];
  };
  const add = (parts, value) => {
    if (!parts.length) { result = value; return; }
    const parent = parentAt(parts);
    const property = key(parent, parts.at(-1), true);
    if (Array.isArray(parent)) parent.splice(property, 0, value);
    else Object.defineProperty(parent, property, { value, enumerable: true, configurable: true, writable: true });
  };
  for (const operation of operations) {
    const target = pointer(operation.path);
    if (operation.op === 'add' || operation.op === 'replace' || operation.op === 'test') {
      if (!Object.hasOwn(operation, 'value')) throw new Error('Value missing');
      if (operation.op === 'test') { if (!isDeepStrictEqual(read(target), operation.value)) throw new Error('Test failed'); }
      else if (operation.op === 'replace') {
        read(target);
        if (!target.length) result = structuredClone(operation.value);
        else {
          const parent = parentAt(target);
          parent[key(parent, target.at(-1), false)] = structuredClone(operation.value);
        }
      } else add(target, structuredClone(operation.value));
    } else if (operation.op === 'remove') remove(target);
    else if (operation.op === 'copy' || operation.op === 'move') {
      const source = pointer(operation.from);
      if (operation.op === 'move' && target.length > source.length && source.every((component, index) => target[index] === component)) throw new Error('Move into descendant');
      const value = structuredClone(read(source));
      if (operation.op === 'move') remove(source);
      add(target, value);
    } else throw new Error('Unknown operation');
  }
  return result;
}
