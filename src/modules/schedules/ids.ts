const alphabet = "0123456789abcdefghijklmnopqrstuvwxyz";

const randomChars = (count: number): string => {
  const bytes = crypto.getRandomValues(new Uint8Array(count));
  let value = "";
  for (const byte of bytes) {
    value += alphabet[byte % alphabet.length];
  }
  return value;
};

export const createId = (prefix: string): string => {
  const time = Date.now().toString(36).padStart(9, "0");
  return `${prefix}_${time}${randomChars(10)}`;
};

// A one-shot job still needs a name, because the name is what its run record
// and its log directory are filed under. It is a slug of the label or the
// executor plus enough entropy that two invocations never collide.
export const createJobName = (label: string): string => {
  const slug = label
    .toLowerCase()
    .replaceAll(/[^a-z0-9]+/gu, "-")
    .slice(0, 24)
    .replaceAll(/^-+|-+$/gu, "");
  return `once-${slug === "" ? "job" : slug}-${randomChars(6)}`;
};
