// SPDX-License-Identifier: 0BSD

///////////////////////////////////////////////////////////////////////////////
//
/// \file       lz_decoder.c
/// \brief      LZ out window
///
//  Authors:    Igor Pavlov
//              Lasse Collin
//
///////////////////////////////////////////////////////////////////////////////

// liblzma supports multiple LZ77-based filters. The LZ part is shared
// between these filters. The LZ code takes care of dictionary handling
// and passing the data between filters in the chain. The filter-specific
// part decodes from the input buffer to the dictionary.


#include "lz_decoder.h"


// ggcat LZ hook: per-thread state copied into the dictionary before every
// decoding step. Thread-local because the LZ coder is not reachable through
// the public lzma_stream API.
#if defined(_MSC_VER)
#	define LZMA_LZ_TLS __declspec(thread)
#else
#	define LZMA_LZ_TLS __thread
#endif

typedef struct {
	lzma_lz_match_hook hook;
	void *ctx;
	uint64_t out_abs_base;
	uint32_t min_len;
	bool suppressed;
} lzma_lz_hook_state;

static LZMA_LZ_TLS lzma_lz_hook_state lz_hook_tls = { NULL, NULL, 0, 0, false };


extern void
lzma_lz_set_match_hook(lzma_lz_match_hook hook, void *ctx,
		uint64_t out_abs_base, uint32_t min_len)
{
	lz_hook_tls.hook = hook;
	lz_hook_tls.ctx = ctx;
	lz_hook_tls.out_abs_base = out_abs_base;
	lz_hook_tls.min_len = min_len;
}


extern bool
lzma_lz_suppress_match_hook(bool suppress)
{
	const bool previous = lz_hook_tls.suppressed;
	lz_hook_tls.suppressed = suppress;
	return previous;
}


// ggcat in-place decoding: the calling thread's state, NULL while it is off.
// See lzma_lz_in_place in lz_decoder.h.
static LZMA_LZ_TLS lzma_lz_in_place *lz_in_place_tls = NULL;


extern void
lzma_lz_set_in_place(lzma_lz_in_place *state)
{
	lz_in_place_tls = state;
}


extern void
lzma_lz_in_place_free(uint8_t *buf)
{
	lzma_free(buf, NULL);
}


extern bool
lzma_lz_in_place_block(bool filtered, const uint8_t *out)
{
	lzma_lz_in_place *state = lz_in_place_tls;
	if (state == NULL)
		return false;

	state->filtered = filtered;
	if (!filtered || state->buf == NULL)
		return false;

	const uintptr_t at = (uintptr_t)out;
	const uintptr_t start = (uintptr_t)state->buf;
	return at >= start && at < start + state->size + LZ_DICT_EXTRA;
}


typedef struct {
	/// Dictionary (history buffer)
	lzma_dict dict;

	/// The actual LZ-based decoder e.g. LZMA
	lzma_lz_decoder lz;

	/// Next filter in the chain, if any. Note that LZMA and LZMA2 are
	/// only allowed as the last filter, but the long-range filter in
	/// future can be in the middle of the chain.
	lzma_next_coder next;

	/// True if the next filter in the chain has returned LZMA_STREAM_END.
	bool next_finished;

	/// True if the LZ decoder (e.g. LZMA) has detected end of payload
	/// marker. This may become true before next_finished becomes true.
	bool this_finished;

	/// Temporary buffer needed when the LZ-based filter is not the last
	/// filter in the chain. The output of the next filter is first
	/// decoded into buffer[], which is then used as input for the actual
	/// LZ-based decoder.
	struct {
		size_t pos;
		size_t size;
		uint8_t buffer[LZMA_BUFFER_SIZE];
	} temp;
} lzma_coder;


/// ggcat in-place decoding: tell the caller where the dictionary is and where
/// the next byte will be decoded.
static void
in_place_report(const lzma_coder *coder)
{
	lzma_lz_in_place *state = lz_in_place_tls;
	if (state == NULL)
		return;

	state->buf = coder->dict.buf;
	state->size = coder->dict.size;

	// A full dictionary wraps before the next byte is decoded.
	state->next = coder->dict.pos == coder->dict.size
			? LZ_DICT_REPEAT_MAX : coder->dict.pos;
	return;
}


/// Free the dictionary or, during in-place decoding, hand it to the caller,
/// which may still be reading the bytes in it.
static void
dict_free(lzma_coder *coder, const lzma_allocator *allocator)
{
	lzma_lz_in_place *state = lz_in_place_tls;
	if (state != NULL && state->retire != NULL && allocator == NULL
			&& coder->dict.buf != NULL) {
		state->retire(state->retire_ctx, coder->dict.buf,
				coder->dict.size);
		if (state->buf == coder->dict.buf) {
			state->buf = NULL;
			state->size = 0;
		}
	} else {
		lzma_free(coder->dict.buf, allocator);
	}

	coder->dict.buf = NULL;
	coder->dict.size = 0;
	return;
}


static void
lz_decoder_reset(lzma_coder *coder)
{
	// ggcat in-place decoding: the bytes decoded so far stay where they
	// are, for the caller to read, and the dictionary restarts right after
	// them. The LZMA decoder takes the position state from the low bits of
	// pos and the previous byte from buf[pos - 1], so the restart is at
	// the next multiple of 16, with a zero in front. A full dictionary
	// restarts at its beginning, where it would wrap anyway; a new one
	// starts at LZ_DICT_INIT_POS, which is also its full_base.
	size_t pos = coder->dict.pos;
	if (pos != coder->dict.full_base)
		pos = pos == coder->dict.size ? LZ_DICT_REPEAT_MAX
				: (pos + 16) & ~((size_t)(15));

	coder->dict.pos = pos;
	coder->dict.full_base = pos;
	coder->dict.buf[pos - 1] = '\0';
	coder->dict.full = 0;
	coder->dict.has_wrapped = false;
	coder->dict.need_reset = false;
	return;
}


static lzma_ret
decode_buffer(lzma_coder *coder,
		const uint8_t *restrict in, size_t *restrict in_pos,
		size_t in_size, uint8_t *restrict out,
		size_t *restrict out_pos, size_t out_size)
{
	while (true) {
		// Wrap the dictionary if needed.
		if (coder->dict.pos == coder->dict.size) {
			// See the comment of #define LZ_DICT_REPEAT_MAX.
			coder->dict.pos = LZ_DICT_REPEAT_MAX;
			memcpy(coder->dict.buf, coder->dict.buf
						+ coder->dict.size
						- LZ_DICT_REPEAT_MAX,
					LZ_DICT_REPEAT_MAX);

			// ggcat in-place decoding: after a reset that left the
			// decoded bytes in place, the dictionary is not full
			// yet when it wraps, and goes on filling up past it.
			if (!coder->dict.has_wrapped) {
				const size_t full = coder->dict.size
						- coder->dict.full_base;
				if (full < coder->dict.size
						- 2 * LZ_DICT_REPEAT_MAX)
					coder->dict.full_base
						= LZ_DICT_REPEAT_MAX - full;
				else
					coder->dict.has_wrapped = true;
			}
		}

		// Store the current dictionary position. It is needed to know
		// where to start copying to the out[] buffer.
		const size_t dict_start = coder->dict.pos;

		// ggcat in-place decoding: decode only straight into the
		// caller's buffer, which is the dictionary at dict_start. If it
		// is not (the position moved), let the caller call again with
		// the right one. The output of a filtered block goes to the
		// filter, through its buffer, copied as upstream does.
		const bool in_place = !lz_hook_tls.suppressed;
		if (in_place && (uintptr_t)out + *out_pos
				!= (uintptr_t)(coder->dict.buf + dict_start))
			return LZMA_OK;

		// Calculate how much we allow coder->lz.code() to decode.
		// It must not decode past the end of the dictionary
		// buffer, and we don't want it to decode more than is
		// actually needed to fill the out[] buffer.
		coder->dict.limit = coder->dict.pos
				+ my_min(out_size - *out_pos,
					coder->dict.size - coder->dict.pos);

		// ggcat in-place decoding: stop where the dictionary becomes
		// full (before its end after a reset in place), to mark it so.
		// Wrapping arithmetic: full_base can be "negative".
		if (!coder->dict.has_wrapped) {
			const size_t full_end = coder->dict.full_base
					+ (coder->dict.size
						- 2 * LZ_DICT_REPEAT_MAX);
			if (full_end - dict_start
					< coder->dict.limit - dict_start)
				coder->dict.limit = full_end;
		}

		// ggcat LZ hook: dict.buf[dict_start..dict.pos] is copied to
		// out[*out_pos..] right after this call, so a dictionary byte at
		// index p lands at out_abs_base + *out_pos + (p - dict_start).
		const bool hooked = lz_hook_tls.hook != NULL
				&& !lz_hook_tls.suppressed;
		coder->dict.hook = hooked ? lz_hook_tls.hook : NULL;
		coder->dict.hook_ctx = lz_hook_tls.ctx;
		coder->dict.hook_min_len = hooked
				? lz_hook_tls.min_len : UINT32_MAX;
		coder->dict.abs_base = lz_hook_tls.out_abs_base
				+ (uint64_t)(*out_pos) - (uint64_t)dict_start;

		// Call the coder->lz.code() to do the actual decoding.
		const lzma_ret ret = coder->lz.code(
				coder->lz.coder, &coder->dict,
				in, in_pos, in_size);

		if (!coder->dict.has_wrapped && coder->dict.pos
				- coder->dict.full_base >= coder->dict.size
					- 2 * LZ_DICT_REPEAT_MAX)
			coder->dict.has_wrapped = true;

		// Copy the decoded data from the dictionary to the out[]
		// buffer. Do it conditionally because out can be NULL
		// (in which case copy_size is always 0). Calling memcpy()
		// with a null-pointer is undefined even if the third
		// argument is 0. Decoding in place, the bytes are there
		// already: only a filtered block's are copied.
		const size_t copy_size = coder->dict.pos - dict_start;
		assert(copy_size <= out_size - *out_pos);

		if (copy_size > 0 && !in_place)
			memcpy(out + *out_pos, coder->dict.buf + dict_start,
					copy_size);

		*out_pos += copy_size;

		// Reset the dictionary if so requested by coder->lz.code().
		if (coder->dict.need_reset) {
			lz_decoder_reset(coder);

			// Since we reset dictionary, we don't check if
			// dictionary became full.
			if (ret != LZMA_OK || *out_pos == out_size)
				return ret;
		} else {
			// Return if everything got decoded or an error
			// occurred, or if there's no more data to decode.
			//
			// Note that detecting if there's something to decode
			// is done by looking if dictionary become full
			// instead of looking if *in_pos == in_size. This
			// is because it is possible that all the input was
			// consumed already but some data is pending to be
			// written to the dictionary.
			if (ret != LZMA_OK || *out_pos == out_size
					|| coder->dict.pos < coder->dict.size)
				return ret;
		}
	}
}


static lzma_ret
lz_decode(void *coder_ptr, const lzma_allocator *allocator,
		const uint8_t *restrict in, size_t *restrict in_pos,
		size_t in_size, uint8_t *restrict out,
		size_t *restrict out_pos, size_t out_size,
		lzma_action action)
{
	lzma_coder *coder = coder_ptr;

	if (coder->next.code == NULL) {
		const lzma_ret ret = decode_buffer(coder, in, in_pos, in_size,
				out, out_pos, out_size);
		in_place_report(coder);
		return ret;
	}

	// We aren't the last coder in the chain, we need to decode
	// our input to a temporary buffer.
	while (*out_pos < out_size) {
		// Fill the temporary buffer if it is empty.
		if (!coder->next_finished
				&& coder->temp.pos == coder->temp.size) {
			coder->temp.pos = 0;
			coder->temp.size = 0;

			const lzma_ret ret = coder->next.code(
					coder->next.coder,
					allocator, in, in_pos, in_size,
					coder->temp.buffer, &coder->temp.size,
					LZMA_BUFFER_SIZE, action);

			if (ret == LZMA_STREAM_END)
				coder->next_finished = true;
			else if (ret != LZMA_OK || coder->temp.size == 0)
				return ret;
		}

		if (coder->this_finished) {
			if (coder->temp.size != 0)
				return LZMA_DATA_ERROR;

			if (coder->next_finished)
				return LZMA_STREAM_END;

			return LZMA_OK;
		}

		const lzma_ret ret = decode_buffer(coder, coder->temp.buffer,
				&coder->temp.pos, coder->temp.size,
				out, out_pos, out_size);

		if (ret == LZMA_STREAM_END)
			coder->this_finished = true;
		else if (ret != LZMA_OK)
			return ret;
		else if (coder->next_finished && *out_pos < out_size)
			return LZMA_DATA_ERROR;
	}

	return LZMA_OK;
}


static void
lz_decoder_end(void *coder_ptr, const lzma_allocator *allocator)
{
	lzma_coder *coder = coder_ptr;

	lzma_next_end(&coder->next, allocator);
	dict_free(coder, allocator);

	if (coder->lz.end != NULL)
		coder->lz.end(coder->lz.coder, allocator);
	else
		lzma_free(coder->lz.coder, allocator);

	lzma_free(coder, allocator);
	return;
}


extern lzma_ret
lzma_lz_decoder_init(lzma_next_coder *next, const lzma_allocator *allocator,
		const lzma_filter_info *filters,
		lzma_ret (*lz_init)(lzma_lz_decoder *lz,
			const lzma_allocator *allocator,
			lzma_vli id, const void *options,
			lzma_lz_options *lz_options))
{
	// Allocate the base structure if it isn't already allocated.
	lzma_coder *coder = next->coder;
	if (coder == NULL) {
		coder = lzma_alloc(sizeof(lzma_coder), allocator);
		if (coder == NULL)
			return LZMA_MEM_ERROR;

		next->coder = coder;
		next->code = &lz_decode;
		next->end = &lz_decoder_end;

		coder->dict.buf = NULL;
		coder->dict.size = 0;
		coder->dict.pos = LZ_DICT_INIT_POS;
		coder->dict.full_base = LZ_DICT_INIT_POS;
		coder->dict.hook = NULL;
		coder->dict.hook_ctx = NULL;
		coder->dict.hook_min_len = UINT32_MAX;
		coder->dict.abs_base = 0;
		coder->lz = LZMA_LZ_DECODER_INIT;
		coder->next = LZMA_NEXT_CODER_INIT;
	}

	// Allocate and initialize the LZ-based decoder. It will also give
	// us the dictionary size.
	lzma_lz_options lz_options;
	return_if_error(lz_init(&coder->lz, allocator,
			filters[0].id, filters[0].options, &lz_options));

	// If the dictionary size is very small, increase it to 4096 bytes.
	// This is to prevent constant wrapping of the dictionary, which
	// would slow things down. The downside is that since we don't check
	// separately for the real dictionary size, we may happily accept
	// corrupt files.
	if (lz_options.dict_size < 4096)
		lz_options.dict_size = 4096;

	// Make dictionary size a multiple of 16. Some LZ-based decoders like
	// LZMA use the lowest bits lzma_dict.pos to know the alignment of the
	// data. Aligned buffer is also good when memcpying from the
	// dictionary to the output buffer, since applications are
	// recommended to give aligned buffers to liblzma.
	//
	// Reserve 2 * LZ_DICT_REPEAT_MAX bytes of extra space which is
	// needed for alloc_size. Reserve also LZ_DICT_EXTRA bytes of extra
	// space which is *not* counted in alloc_size or coder->dict.size.
	//
	// Avoid integer overflow.
	if (lz_options.dict_size > SIZE_MAX - 15 - 2 * LZ_DICT_REPEAT_MAX
			- LZ_DICT_EXTRA)
		return LZMA_MEM_ERROR;

	lz_options.dict_size = (lz_options.dict_size + 15) & ~((size_t)(15));

	// Reserve extra space as explained in the comment
	// of #define LZ_DICT_REPEAT_MAX.
	size_t alloc_size = lz_options.dict_size + 2 * LZ_DICT_REPEAT_MAX;

	// ggcat in-place decoding: this fork decodes in place only, with the
	// caller's state. The dictionary is at least as large as the caller
	// needs, and one large enough is kept, so that the bytes decoded so far
	// stay readable; one too small is handed to the caller (dict_free()).
	const lzma_lz_in_place *in_place = lz_in_place_tls;
	if (in_place == NULL)
		return LZMA_PROG_ERROR;

	if (lz_options.preset_dict != NULL && lz_options.preset_dict_size > 0)
		return LZMA_OPTIONS_ERROR;

	if (in_place->min_size > SIZE_MAX - 15 - LZ_DICT_EXTRA)
		return LZMA_MEM_ERROR;

	const size_t min_size = (in_place->min_size + 15) & ~((size_t)(15));
	if (alloc_size < min_size)
		alloc_size = min_size;

	if (coder->dict.buf != NULL && coder->dict.size >= alloc_size)
		alloc_size = coder->dict.size;

	// Allocate and initialize the dictionary.
	if (coder->dict.size != alloc_size) {
		dict_free(coder, allocator);

		// The LZ_DICT_EXTRA bytes at the end of the buffer aren't
		// included in alloc_size. These extra bytes allow
		// dict_repeat() to read and write more data than requested.
		// Otherwise this extra space is ignored.
		coder->dict.buf = lzma_alloc(alloc_size + LZ_DICT_EXTRA,
				allocator);
		if (coder->dict.buf == NULL)
			return LZMA_MEM_ERROR;

		// NOTE: Yes, alloc_size, not lz_options.dict_size. The way
		// coder->dict.full is updated will take care that we will
		// still reject distances larger than lz_options.dict_size.
		coder->dict.size = alloc_size;

		// A new dictionary starts at the beginning.
		coder->dict.pos = LZ_DICT_INIT_POS;
		coder->dict.full_base = LZ_DICT_INIT_POS;
	}

	lz_decoder_reset(next->coder);
	in_place_report(next->coder);

	// Miscellaneous initializations
	coder->next_finished = false;
	coder->this_finished = false;
	coder->temp.pos = 0;
	coder->temp.size = 0;

	// Initialize the next filter in the chain, if any.
	return lzma_next_filter_init(&coder->next, allocator, filters + 1);
}


extern uint64_t
lzma_lz_decoder_memusage(size_t dictionary_size)
{
	return sizeof(lzma_coder) + (uint64_t)(dictionary_size)
			+ 2 * LZ_DICT_REPEAT_MAX + LZ_DICT_EXTRA;
}
