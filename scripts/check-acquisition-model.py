#!/usr/bin/env python3
"""Explore a bounded SC model before implementing shared claims.

Two consumers select the same generation, with independently held leases.
One producer can retire/scan/reuse twice; an optional closer races acquisition.
Every claim store, validation, scan, read, release and close is a separate step.
Negative controls must find counterexamples or the checker is insensitive.
This models the ownership protocol, not Rust's memory model or GPU execution.
"""

from dataclasses import dataclass, replace


@dataclass(frozen=True)
class State:
    consumer_pc: tuple = (0, 0)
    claims: tuple = (0, 0)
    validated: tuple = (False, False)
    admitted: tuple = (False, False)
    generation: int = 1
    payload_generation: int = 1
    published: bool = True
    producer_pc: int = 0
    producer_cycle: int = 0
    observed_claim: bool = False
    active: tuple = (True, True)
    closed: tuple = (False, False)


def changed(values, index, value):
    return values[:index] + (value,) + values[index + 1:]


def transitions(state, mode, with_close):
    for index, pc in enumerate(state.consumer_pc):
        if pc == 5:
            continue
        next_state = replace(state, consumer_pc=changed(state.consumer_pc, index, pc + 1))
        label = f"consumer {index}: "
        if pc == 0:
            next_state = replace(next_state, claims=changed(state.claims, index, 1))
            label += "claim generation 1"
        elif pc == 1:
            valid = state.published and state.generation == 1
            next_state = replace(next_state, validated=changed(state.validated, index, valid))
            label += f"validate resource ({valid})"
        elif pc == 2:
            admitted = state.active[index] and state.validated[index]
            next_state = replace(next_state, admitted=changed(state.admitted, index, admitted))
            if not admitted or mode == "early-release":
                next_state = replace(next_state, claims=changed(state.claims, index, 0))
            label += f"validate incarnation ({admitted})"
        elif pc == 3:
            if state.admitted[index] and state.payload_generation != 1:
                yield label + "READ REUSED STORAGE", None
                continue
            label += "read under lease" if state.admitted[index] else "miss (no read)"
        elif pc == 4:
            next_state = replace(next_state, claims=changed(state.claims, index, 0))
            label += "release after use"
        yield label, next_state

    if with_close:
        for index, closed in enumerate(state.closed):
            if not closed:
                yield f"close incarnation {index} (keep claims)", replace(
                    state, active=changed(state.active, index, False), closed=changed(state.closed, index, True)
                )

    if state.producer_cycle == 2:
        return
    pc = state.producer_pc
    next_state = replace(state, producer_pc=pc + 1)
    if mode == "scan-before-retire":
        retire_pc, scans = 2, (0, 1)
    else:
        retire_pc, scans = 0, (1, 2)
    if pc == retire_pc:
        next_state = replace(next_state, published=False)
        label = "producer: retire"
    elif pc in scans:
        index = scans.index(pc)
        observed = state.claims[index] == state.generation
        next_state = replace(next_state, observed_claim=state.observed_claim or observed)
        label = f"producer: scan claim {index} ({observed})"
    elif pc == 3:
        if not state.observed_claim:
            next_state = replace(next_state, payload_generation=state.payload_generation + 1)
        label = "producer: blocked by claim" if state.observed_claim else "producer: WRITE"
    elif pc == 4:
        if not state.observed_claim:
            generation = 1 if mode == "reuse-generation" else state.generation + 1
            next_state = replace(next_state, generation=generation, published=True)
        next_state = replace(next_state, producer_pc=0, producer_cycle=state.producer_cycle + 1, observed_claim=False)
        label = "producer: finish reuse attempt"
    yield label, next_state


def explore(mode, with_close):
    visited = set()
    stack = [(State(), ())]
    while stack:
        state, trace = stack.pop()
        if state in visited:
            continue
        visited.add(state)
        for label, next_state in transitions(state, mode, with_close):
            if next_state is None:
                return len(visited), trace + (label,)
            stack.append((next_state, trace + (label,)))
    return len(visited), None


def main():
    for with_close in (False, True):
        count, failure = explore("correct", with_close)
        if failure:
            raise AssertionError("\n".join(failure))
        print(f"claim-before-validate, close={with_close}: {count} states, no unsafe read")
    for mode in ("scan-before-retire", "reuse-generation", "early-release"):
        count, failure = explore(mode, True)
        if not failure:
            raise AssertionError(f"negative control {mode} found no counterexample")
        print(f"negative control {mode}: counterexample after {count} states")
        for step in failure:
            print(f"  {step}")


if __name__ == "__main__":
    main()
