/*
 * The vDSO's layout (servers/linux/vdso/vdso.rs, ADR 0014), after Linux's
 * arch/x86/entry/vdso/vdso-layout.lds.S: one read-only, executable PT_LOAD
 * from address 0 that holds the ELF and program headers, the dynamic symbols
 * with their versions, the dynamic section, the unwind tables and the code;
 * no writable data, nothing to relocate. The kernel's clock page is mapped
 * one page below it ([vvar]).
 */

__vdso_clock_page = . - 4096;

SECTIONS
{
	. = SIZEOF_HEADERS;

	.hash		: { *(.hash) }			:text
	.gnu.hash	: { *(.gnu.hash) }
	.dynsym		: { *(.dynsym) }
	.dynstr		: { *(.dynstr) }
	.gnu.version	: { *(.gnu.version) }
	.gnu.version_d	: { *(.gnu.version_d) }
	.gnu.version_r	: { *(.gnu.version_r) }

	.dynamic	: { *(.dynamic) }		:text	:dynamic

	.rodata		: { *(.rodata .rodata.*) }	:text

	.eh_frame_hdr	: { *(.eh_frame_hdr) }		:text	:eh_frame_hdr
	.eh_frame	: { KEEP (*(.eh_frame)) }	:text

	.text		: { *(.text .text.*) }		:text	=0x90909090

	/DISCARD/ : {
		*(.data .data.* .bss .bss.* .got .got.* .plt .plt.* .comment .note.*)
	}
}

PHDRS
{
	text		PT_LOAD		FLAGS(5) FILEHDR PHDRS;	/* PF_R | PF_X */
	dynamic		PT_DYNAMIC	FLAGS(4);		/* PF_R */
	eh_frame_hdr	PT_GNU_EH_FRAME;
}
