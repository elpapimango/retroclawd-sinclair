; Scenario program for the ZX Spectrum 48K (S4): a steady square tone on the beeper (port FEh bit 4). One half period is
; OUT 11 + XOR 7 + LD B 7 + DJNZ 256 x 13 - 5 + JR 12 = 3360 T-states, so the pitch is clock / 6720 (about 520 Hz).
        .cpu z80
        .org $8000
start:  di
        ld a,$10
loop:   out ($fe),a
        xor $10
        ld b,0
wait:   djnz wait
        jr loop
