; Scenario program for the ZX Spectrum 128K / +2 / +2A / +3 (S4): channel A of the AY-3-8910 plays a steady tone. The chip's
; clock is 1.7734475 MHz and a tone is clock / (16 x period), so period 213 gives about 520 Hz.
        .cpu z80
        .org $8000
start:  di
        ld hl,regs
next:   ld a,(hl)
        cp $ff
        jr z,done
        ld bc,$fffd
        out (c),a
        inc hl
        ld a,(hl)
        ld b,$bf
        out (c),a
        inc hl
        jr next
done:   halt
        jr done
; register, value pairs: tone A period low / high, mixer (tone A on, everything else off), volume A, end
regs:   .byte 0,213, 1,0, 7,%00111110, 8,15, $ff
