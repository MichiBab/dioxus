// State belongs to the interpreter, not a socket. An ACK can be lost AFTER
// mutations were applied; retrying that batch must only resend its ACK.
export class MutationSequence {
    private applied: bigint | undefined;
    private failed = false;

    apply(
        frame: ArrayBuffer,
        mutate: (bytes: ArrayBuffer) => void,
    ): ArrayBuffer {
        if (this.failed)
            throw new Error("Mutation interpreter requires reinitialization");
        if (frame.byteLength < 8) throw new Error("Truncated mutation frame");
        const id = new DataView(frame).getBigUint64(0, true);
        if (this.applied !== undefined && id <= this.applied) {
            return frame.slice(0, 8);
        }
        if (this.applied !== undefined && id !== this.applied + BigInt(1)) {
            throw new Error("Out-of-order mutation frame");
        }
        try {
            mutate(frame.slice(8));
        } catch (error) {
            // Never retry a partially applied, non-idempotent batch.
            this.failed = true;
            throw error;
        }
        this.applied = id;
        return frame.slice(0, 8);
    }
}
