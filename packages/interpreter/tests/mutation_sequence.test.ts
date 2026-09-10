import { describe, expect, test } from "bun:test";
import { MutationSequence } from "../src/ts/mutation_sequence";

function frame(id: number, payload = 42): ArrayBuffer {
    const bytes = new ArrayBuffer(9);
    new DataView(bytes).setBigUint64(0, BigInt(id), true);
    new Uint8Array(bytes)[8] = payload;
    return bytes;
}

describe("mutation acknowledgements", () => {
    test("lost ACK: replay acknowledges without applying twice", () => {
        const sequence = new MutationSequence();
        const applied: number[] = [];
        const mutate = (bytes: ArrayBuffer) =>
            applied.push(new Uint8Array(bytes)[0]);
        sequence.apply(frame(1), mutate); // DOM changed, ACK lost
        const ack = sequence.apply(frame(1), mutate); // replacement socket
        expect(new DataView(ack).getBigUint64(0, true)).toBe(BigInt(1));
        sequence.apply(frame(2, 43), mutate);
        expect(applied).toEqual([42, 43]);
    });

    test("delayed older frame cannot mutate newer DOM", () => {
        const sequence = new MutationSequence();
        let applies = 0;
        sequence.apply(frame(1), () => applies++);
        sequence.apply(frame(2), () => applies++);
        sequence.apply(frame(1), () => applies++);
        expect(applies).toBe(2);
    });

    test("rejects truncated and out-of-order frames before applying", () => {
        const sequence = new MutationSequence();
        let applies = 0;
        expect(() =>
            sequence.apply(new ArrayBuffer(7), () => applies++),
        ).toThrow();
        sequence.apply(frame(1), () => applies++);
        expect(() => sequence.apply(frame(3), () => applies++)).toThrow(
            "Out-of-order",
        );
        expect(applies).toBe(1);
    });

    test("a partially applied batch must not be replayed or acknowledged", () => {
        const sequence = new MutationSequence();
        let applies = 0;
        expect(() =>
            sequence.apply(frame(1), () => {
                applies++;
                throw new Error("bad mutation");
            }),
        ).toThrow("bad mutation");
        expect(() => sequence.apply(frame(1), () => applies++)).toThrow(
            "reinitialization",
        );
        expect(applies).toBe(1);
    });
});
