MEMORY {
    BOOT2 : ORIGIN = 0x10000000, LENGTH = 0x100
    FLASH : ORIGIN = 0x10000100, LENGTH = 2048K - 0x100
    RAM   : ORIGIN = 0x20000000, LENGTH = 256K
}

SECTIONS {
    /* Boot second stage must be the first 256 bytes of flash. */
    .boot2 ORIGIN(BOOT2) : { KEEP(*(.boot2)) } > BOOT2
} INSERT BEFORE .text;

/* RAM-resident flash routines: the CPU cannot fetch from flash while the
   SSI is in command mode, so these must live in RAM. */
SECTIONS {
    .data.ram_func : {
        *(.data.ram_func .data.ram_func.*)
    } > RAM AT> FLASH
} INSERT AFTER .data;
