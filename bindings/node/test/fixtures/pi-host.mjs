// Stands in for pi's Bun binary (`bun build --compile`): loads the extension
// at argv[2] from disk through jiti/static with tryNative off, as pi's
// extension loader does in that binary, and runs its default export.
import { createJiti } from "jiti/static";

const bunfs = ["$bunfs", "~BUN", "%7EBUN"].some((marker) => import.meta.url.includes(marker));
console.log(`host: compiled ${bunfs}`);
const jiti = createJiti(import.meta.url, { moduleCache: false, tryNative: false });
const extension = await jiti.import(process.argv[2], { default: true });
await extension();
