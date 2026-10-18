; Scenario program for the ZX Spectrum 48K: red border, black on white, HELLO in the top left cell row, then halts.
; Draws straight into screen memory with glyphs from the ROM font (3D00h): a snapshot starts without the ROM's
; system variables, so no ROM routine is called.
        .cpu z80
        .org $8000
start:  di
        ld a,2
        out ($fe),a
        ld hl,$5800
        ld de,$5801
        ld bc,767
        ld (hl),$38
        ldir
        ld hl,msg
        ld de,$4000
next:   ld a,(hl)
        or a
        jr z,done
        push hl
        ld l,a
        ld h,0
        add hl,hl
        add hl,hl
        add hl,hl
        ld bc,$3C00
        add hl,bc
        push de
        ld b,8
row:    ld a,(hl)
        ld (de),a
        inc hl
        inc d
        djnz row
        pop de
        inc de
        pop hl
        inc hl
        jr next
done:   halt
        jr done
msg:    .asciiz "HELLO"
