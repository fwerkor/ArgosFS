# Wire format

**Status:** stable as of `reed-solomon-engine` 0.1.0. Any change to anything in this document is a semver-major change. Three pieces of work during 0.1.0 development changed how the bytes are produced and none of what they are: the SIMD kernels, the closed-form generator-matrix construction of section 4.1, and the blocked multi-row encode kernel. None required a change to any normative part of this document, which is the outcome section 10 is designed to guarantee.

This document specifies the bytes `reed-solomon-engine` produces, in enough detail to reimplement from scratch without reading the source. It is normative. If the code and this document ever disagree, one of them is a bug, and the known-answer vectors in `tests/` decide which.

Sender and receiver frequently run different versions of an erasure library. If the generator matrix changes between versions, parity shards silently stop reconstructing and no ordinary unit test catches it. That is why the construction is written down here in prose rather than left as "whatever the code does".

---

## 1. The field

All arithmetic is in GF(2^8), the finite field with 256 elements.

**Representation.** A field element is one byte. Bit `i` of the byte (with bit 0 the least significant) is the coefficient of `x^i` in a polynomial over GF(2) of degree at most 7. So `0x03` is the polynomial `x + 1` and `0x80` is `x^7`.

**Addition** is bitwise XOR. Subtraction is the same operation, so `a - b == a + b == a ^ b` and every sign in the linear algebra below can be ignored.

**Multiplication** is polynomial multiplication over GF(2) followed by reduction modulo

```
p(x) = x^8 + x^4 + x^3 + x^2 + 1
```

which is `0x11D` as a 9-bit value. Note this is **not** the AES polynomial `0x11B`. Choosing `0x11D` is what makes this crate interchangeable with other implementations of the standard GF(2^8) erasure-coding format.

Reference multiplication, using no tables:

```
mul(a, b):
    product = 0                       # 16-bit accumulator
    for bit in 0..8:
        if b & (1 << bit):
            product ^= a << bit
    for bit in 15 down to 8:
        if product & (1 << bit):
            product ^= 0x11D << (bit - 8)
    return product as a byte
```

An implementation is free to use logarithm tables instead; this crate does. The table-driven and table-free forms must agree for all 65536 input pairs, and the crate tests exactly that.

**Primitive element.** `0x02` (the polynomial `x`) generates the multiplicative group. The logarithm table is defined by `exp[0] = 1` and `exp[i+1] = mul(exp[i], 0x02)`, with `log[exp[i]] = i` for `i` in `0..255`. This choice affects only how an implementation computes products, never the products themselves, but it is stated here because it fixes the table layout that every reference implementation shares.

**Exponentiation** is repeated multiplication, with one convention that matters:

```
pow(a, 0) = 1   for every a, including a = 0
pow(0, n) = 0   for n > 0
```

`pow(0, 0) = 1` is not an arbitrary choice. It is what makes row 0 of the Vandermonde matrix `[1, 0, 0, ...]`, and every implementation this format is compatible with does the same thing. Getting it wrong changes the first row of the generator matrix and therefore every parity byte.

---

## 2. Shard layout

A codec is parameterised by two counts:

- `k` = `data_shards`, at least 1
- `m` = `parity_shards`, at least 1
- `n` = `k + m` = `total_shards`, at most **256**

The 256 limit comes from the field: the construction needs one distinct field element per shard and GF(2^8) has 256 of them.

Shards are indexed `0 .. n`. **Indices `0 .. k` are the data shards, in order, and indices `k .. n` are the parity shards, in order.** Every shard, data and parity alike, has the same length `L >= 1` bytes. Nothing in the format depends on `L`: shard byte `t` is encoded independently of shard byte `t'`.

This ordering is part of the wire format. A receiver that permutes shards decodes garbage.

---

## 3. The Vandermonde matrix

Let `V` be the `n x k` matrix over GF(2^8) with

```
V[r][c] = pow(r, c)        for r in 0..n, c in 0..k
```

where the row index `r` is reinterpreted as a field element, that is, as the byte with value `r`. Because `n <= 256`, the rows use `n` distinct field elements.

Rows are shards, columns are data-shard positions. (Some texts write the transpose; this orientation is the one used here and by all the compatible implementations.)

Every `k x k` submatrix of `V` formed by choosing `k` distinct rows is a Vandermonde matrix on distinct nodes and is therefore non-singular. That is the MDS property, and it is the reason any `k` surviving shards suffice to recover the message.

---

## 4. Systematic reduction

`V` on its own is not systematic: encoding with it would replace the data shards rather than append to them. Define

```
T = the top k x k block of V, that is, rows 0..k
G = V * T^-1                                        (an n x k matrix)
```

`T` is a `k x k` Vandermonde submatrix on distinct nodes, so `T^-1` exists. Right-multiplying by an invertible matrix does not change the column space, so `G` generates the same MDS code as `V`, and by construction the top `k x k` block of `G` is the identity. The code is therefore systematic: data shards pass through unchanged.

Define the **parity matrix**

```
P = the bottom m rows of G, that is, rows k..n
```

`P` is `m x k`. It is the only thing an encoder needs, and it is what `Codec::parity_matrix()` and `ParityMatrix::coefficients()` return, stored row-major.

Because the code is MDS, *every* square submatrix of `P` is non-singular. That is what allows recovery from any erasure pattern within the correction limit.

The definition above is normative; how an implementation arrives at it is not. Gauss-Jordan elimination over GF(2^8) on the augmented matrix `[T | I]` is the direct route, and it is what a reimplementer should reach for first. Over a finite field there is no numerical stability to worry about, so any non-zero pivot may be chosen, and the choice cannot change `T^-1`, only the path taken to it. `T^-1` is unique, so every route agrees byte for byte.

This crate takes a different route and never forms `T^-1` at all; section 4.1 gives it, because it is also the shortest proof that the matrix is MDS.

### 4.1 `P` in closed form, and why it is MDS

`T` is a Vandermonde matrix, and the inverse of a Vandermonde matrix is Lagrange interpolation written as a matrix: `T^-1[l][c]` is the coefficient of `t^l` in `L_c(t)`, the Lagrange basis polynomial for node `x_c = c`. Row `k + j` of `V` evaluates the same monomials at `y_j = k + j`, so the product collapses to an evaluation:

```
P[j][c] = (V * T^-1)[k + j][c] = L_c(y_j)
        = master(y_j) / (y_j ^ c) / denominator(c)

master(y)      = prod over m < k of (y ^ m)
denominator(c) = prod over m < k, m != c of (c ^ m)
```

Addition is XOR, so a difference of nodes `x_a + x_b` is just `a ^ b`. Every division here is by a non-zero element: `y_j >= k` is greater than every `c`, so `y_j ^ c` is non-zero, and `denominator(c)` is a product of non-zero factors in a field.

Now write `a_j = master(y_j)` and `b_c = 1 / denominator(c)`, both non-zero. Then

```
P[j][c] = a_j * b_c / (y_j ^ c)          that is,   P = diag(a) * C * diag(b)
```

where `C[j][c] = 1 / (y_j + x_c)` is the **Cauchy matrix** on the two disjoint node sets `{y_j}` and `{x_c}`. So the systematic Vandermonde parity matrix *is* a generalized Cauchy matrix; the two constructions people usually present as alternatives are the same matrix here, and the systematic reduction is what turns one into the other.

That gives the MDS property in one line. Every square submatrix of a Cauchy matrix is itself a Cauchy matrix on a subset of the nodes, and its determinant is

```
prod over t < t' of (y_t + y_t') * prod over c < c' of (x_c + x_c')
--------------------------------------------------------------------
                prod over t, c of (y_t + x_c)
```

which is non-zero whenever the nodes are distinct, as they are: the `n <= 256` row indices are distinct field elements. Multiplying rows and columns by the non-zero scalars `a_j` and `b_c` scales the determinant by a non-zero factor and cannot make it zero. Therefore every square submatrix of `P` is non-singular, which is exactly what section 6 needs.

The 4+2 example in section 7 checks out against this directly. With `k = 4`, `master(4) = master(5) = 0x75` and `denominator(c) = 0x06` for every `c`, so `b_c = 0x7A`, and `P[0][0] = (0x75 / 0x04) * 0x7A = 0x1B`. The `0x7A` is visible in section 7.2 as the whole last row of `T^-1`: those entries are the coefficients of `t^3` in each `L_c`, which are exactly the `1 / denominator(c)`.

---

## 5. The encode equation

Write `d_0 .. d_{k-1}` for the data shards and `p_0 .. p_{m-1}` for the parity shards, each an `L`-byte vector. Then

```
p_j = XOR over c in 0..k of ( P[j][c] * d_c )       for j in 0..m
```

where `P[j][c] * d_c` means multiplying every byte of `d_c` by the field scalar `P[j][c]`. Equivalently, byte by byte:

```
p_j[t] = mul(P[j][0], d_0[t]) ^ mul(P[j][1], d_1[t]) ^ ... ^ mul(P[j][k-1], d_{k-1}[t])
```

for every byte offset `t` in `0..L`. Shard index `k + j` on the wire holds `p_j`.

That is the entire wire format. Everything else in this crate is an efficiency question.

---

## 6. Decoding

Decoding is **not** part of the wire format. Any procedure that solves the linear system produces the same answer, because the answer is unique. It is sketched here only so a reimplementer can check their understanding.

Let `M` be the set of missing data-shard indices and `e = |M|`. Recovery requires at least `k` shards present, which guarantees at least `e` surviving parity shards. Pick any `e` of them, with matrix rows `r_0 .. r_{e-1}`. Each contributes one equation:

```
XOR over u in M of ( P[r_t][u] * d_u )  =  p_{r_t}  XOR  ( XOR over present data i of P[r_t][i] * d_i )
```

The `e x e` coefficient matrix `A` with `A[t][u] = P[r_t][M[u]]` is a square submatrix of `P` and is therefore non-singular. Inverting it and applying it to the right-hand side recovers the missing data shards. Missing parity shards are then recomputed with the encode equation of section 5.

Note that the system is `e x e`, not `k x k`. The cost of the linear algebra scales with the number of erasures, not with the code size.

---

## 7. Worked example: 4 data shards, 2 parity shards

Every value below is machine-verified two ways: derived from this document by an independent implementation, and produced by `reed-solomon-engine`.

### 7.1 The Vandermonde matrix

`k = 4`, `m = 2`, `n = 6`. `V[r][c] = pow(r, c)`:

| row `r` | `c=0` | `c=1` | `c=2` | `c=3` |
|---|---|---|---|---|
| 0 | `0x01` | `0x00` | `0x00` | `0x00` |
| 1 | `0x01` | `0x01` | `0x01` | `0x01` |
| 2 | `0x01` | `0x02` | `0x04` | `0x08` |
| 3 | `0x01` | `0x03` | `0x05` | `0x0F` |
| 4 | `0x01` | `0x04` | `0x10` | `0x40` |
| 5 | `0x01` | `0x05` | `0x11` | `0x55` |

Row 0 is `[1, 0, 0, 0]` because of the `pow(0, 0) = 1` convention. Row 3 is worth checking by hand: `3^2 = mul(0x03, 0x03) = (x+1)^2 = x^2 + 1 = 0x05`, and `3^3 = mul(0x05, 0x03) = (x^2+1)(x+1) = x^3 + x^2 + x + 1 = 0x0F`. No reduction is needed at this size.

### 7.2 The inverse of the top block

`T` is rows 0 to 3 of `V`. This crate does not compute this matrix, but it is the shortest route by hand and it is what section 4 defines. Gauss-Jordan on `[T | I]` gives

```
T^-1 =
  0x01  0x00  0x00  0x00
  0x7B  0x01  0x8E  0xF4
  0x00  0x7A  0xF4  0x8E
  0x7A  0x7A  0x7A  0x7A
```

Spot check, row 1 of `T` times column 0 of `T^-1`, which must be 0 because it is off the diagonal: `T` row 1 is `[1, 1, 1, 1]`, so the product is `0x01 ^ 0x7B ^ 0x00 ^ 0x7A`. Since `0x7B ^ 0x7A = 0x01`, the result is `0x00`.

### 7.3 The parity matrix

```
P = bottom 2 rows of (V * T^-1) =
  0x1B  0x1C  0x12  0x14
  0x1C  0x1B  0x14  0x12
```

In decimal: `[27, 28, 18, 20]` and `[28, 27, 20, 18]`. Row-major, that is the byte string `1B 1C 12 14 1C 1B 14 12`, which is exactly what `Codec::new(4, 2)?.parity_matrix()` returns.

Spot check of `P[0][0]`, which is row 4 of `V` dotted with column 0 of `T^-1`:

```
mul(0x01, 0x01) ^ mul(0x04, 0x7B) ^ mul(0x10, 0x00) ^ mul(0x40, 0x7A)
```

Doubling `0x7B` gives `0xF6`; doubling again gives `0x1EC`, which reduces to `0xEC ^ 0x1D = 0xF1`, so `mul(0x04, 0x7B) = 0xF1`. Doubling `0x7A` six times gives, in order, `0xF4`, `0xF5`, `0xF7`, `0xF3`, `0xFB`, `0xEB`, so `mul(0x40, 0x7A) = 0xEB`. Then `0x01 ^ 0xF1 ^ 0x00 ^ 0xEB = 0x1B`.

### 7.4 A one-byte encode, checkable entirely by hand

Data shards of length 1: `d = [0x01, 0x02, 0x03, 0x04]`.

```
p_0 = mul(0x1B, 0x01) ^ mul(0x1C, 0x02) ^ mul(0x12, 0x03) ^ mul(0x14, 0x04)
    = 0x1B ^ 0x38 ^ 0x36 ^ 0x50
    = 0x45

p_1 = mul(0x1C, 0x01) ^ mul(0x1B, 0x02) ^ mul(0x14, 0x03) ^ mul(0x12, 0x04)
    = 0x1C ^ 0x36 ^ 0x3C ^ 0x48
    = 0x5E
```

The full six-shard frame is therefore `01 02 03 04 45 5E`.

### 7.5 A five-byte encode

Data shards `"alpha"`, `"bravo"`, `"charl"`, `"delta"` in ASCII:

| shard | bytes |
|---|---|
| 0 (data) | `61 6C 70 68 61` |
| 1 (data) | `62 72 61 76 6F` |
| 2 (data) | `63 68 61 72 6C` |
| 3 (data) | `64 65 6C 74 61` |
| 4 (parity) | `25 E5 33 39 03` |
| 5 (parity) | `3E 91 6A 77 07` |

Each parity byte is the section 5 equation applied at one offset, independently of the others.

### 7.6 A decode

Suppose data shards 0 and 2 are lost and shards 1, 3, 4, 5 survive. `M = {0, 2}` and `e = 2`. Using both surviving parity rows,

```
A = [ P[0][0]  P[0][2] ]  =  [ 0x1B  0x12 ]
    [ P[1][0]  P[1][2] ]     [ 0x1C  0x14 ]

A^-1 = [ 0xD3  0x8E ]
       [ 0xF4  0xC9 ]
```

Check the top-left entry of `A * A^-1`: `mul(0x1B, 0xD3) ^ mul(0x12, 0xF4) = 0x0F ^ 0x0E = 0x01`, as required.

Applying `A^-1` to the right-hand side of section 6 recovers `"alpha"` and `"charl"`.

---

## 8. Additional matrices, for cross-checking an implementation

`k = 8`, `m = 4`:

```
P[0] = 1A 84 BA 33 E7 10 C6 27
P[1] = 84 1A 33 BA 10 E7 27 C6
P[2] = BA 33 1A 84 C6 27 E7 10
P[3] = 33 BA 84 1A 27 C6 10 E7
```

`k = 17`, `m = 5`:

```
P[0] = 94 94 73 73 DD DD 30 30 E3 E3 EE EE 57 57 51 51 01
P[1] = D7 30 D7 30 68 85 68 85 74 79 74 79 EC EA EC EA 01
P[2] = A4 43 43 A4 58 B5 B5 58 9A 97 97 9A BD BB BB BD 01
P[3] = FE B7 41 02 FE B7 41 02 B3 07 2B 94 B3 07 2B 94 01
P[4] = 23 6A 71 32 6A 23 32 71 E4 50 7A C5 50 E4 C5 7A 01
```

The symmetry visible in these matrices is a property of the construction over a characteristic-2 field, not a coincidence, but nothing in the format depends on it.

---

## 9. Compatibility

The parity bytes produced by this crate are byte-identical to any implementation of the construction specified in this document: the systematic Vandermonde generator matrix over GF(2^8) with reduction polynomial `0x11D`, applied byte-wise. This is the de facto interchange format for GF(2^8) Reed-Solomon erasure coding, and implementations of it exist in several languages.

Compatibility is a property of the format, not of any particular pairing, and this repository verifies its half of it: that this crate matches **this document**, which states the construction in full and in enough detail to reimplement from. Section 7 works a 4+2 example byte by byte for exactly that purpose, and `tests/vectors/` pins the bytes across a spread of parameters. Cross-language interoperability cannot be tested from a Rust test suite, so if you are relying on it, generate vectors from your side and check them against `tests/vectors/` before you ship.

Compatibility is *not* claimed with the GF(2^16) additive-FFT family, which uses a different field and a different code, nor with the Cauchy, PAR1 or Jerasure matrix variants, which are different constructions over the same field.

---

## 10. What counts as a breaking change

Any of the following changes the bytes on the wire and is a **semver-major** change requiring a loud changelog entry:

1. The reduction polynomial.
2. The definition of `V`, including the row-index-to-field-element mapping, the `pow(0, 0) = 1` convention, and the row/column orientation.
3. Using a different square block of `V` for the systematic reduction, or a different systematic construction entirely (Cauchy, Jerasure, PAR1).
4. The ordering of shards, or of the rows of `P`.
5. The maximum shard count, if it were ever raised past 256 by moving to a larger field.

The following are explicitly **not** breaking changes, and the format is designed so they can happen freely:

- How `mul` is implemented, including SIMD kernels, as long as the results are identical.
- How `P` is computed: by inverting `T` with any pivot selection, or by the closed form of section 4.1, which never forms `T^-1` at all. `T^-1` is unique, so every route that computes `V * T^-1` produces the same bytes.
- How the decoder solves for missing shards.
- Which shards the decoder chooses to use when more than the minimum survive.
- Adding a second engine, **provided it produces identical parity bytes for the parameters it shares with the matrix engine**. If two engines cannot be made byte-identical, the engine choice becomes part of the wire format and must be an explicit, caller-visible, serialized parameter rather than an internal heuristic.

The last point is the easiest one to get wrong and the most expensive to discover in production.

---

## 11. Regression barrier

The binding artefacts, in order of authority:

1. The known-answer vectors in `tests/`, which pin fixed inputs to fixed expected parity bytes across a spread of parameters and run on every CI job.
2. This document.

The construction has a second, independent guard. The closed form of section 4.1 is what ships, and the Gauss-Jordan transcription of section 4 is retained as a test-only reference; the two are asserted byte-identical for all 32,640 supported shard splits. A change to either that moved a single coefficient would fail that test before it ever reached the vectors. The two constructions share no code, so this catches a wrong coefficient rather than merely a changed one.

Every SIMD kernel this crate ships is *required* to be byte-identical to its scalar reference kernel, which is in turn checked against the table-free `mul` of section 1 for all 65536 input pairs. Two things enforce it: the cross-kernel tests compare each kernel against that reference for all 256 field scalars over shard lengths covering every vector width and tail remainder, and the known-answer vectors above are checked with whichever kernel the machine running them detects. A vector kernel that disagreed by one byte on one tail length would fail this document's vectors, not merely a unit test.
