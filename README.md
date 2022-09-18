Measuring CPU core-to-core latency
==================================

![License](https://img.shields.io/badge/license-MIT-green.svg)
[![Cargo](https://img.shields.io/crates/v/core-to-core-latency.svg)](https://crates.io/crates/core-to-core-latency)


We measure the latency it takes for a CPU to send a message to another CPU via
its cache coherence protocol.

By pinning two threads on two different CPU cores, we can get them to do a bunch
of compare-exchange operation, and measure the latency.

Results
-------

CPU                                                                  | Release Date  | Median inter-core latency
---------------------------------------------------------------------|---------------| --------------------------
Intel Xeon Platinum 8375C @ 2.90GHz 32 Cores (Ice Lake, 3rd gen)     |  2021-Q2      | 51ns
Intel Xeon Platinum 8275CL @ 3.00GHz 24 Cores (Cascade Lake, 2nd gen)|  2019-Q2      | 47ns
Intel Core i9-9900K @ 3.60 GHz 8 Cores (Coffee Lake, 9th gen)        |  2018-Q4      | 21ns
Intel Xeon E5-2695 v4 @ 2.10GHz 18 Cores (Broadwell, 5th gen)        |  2016-Q1      | 44ns
Intel Core i5-4590 CPU @ 3.30GHz 4 Cores (Haswell, 4th gen)          |  2014-Q2      | 21ns
AMD EPYC 7R13 @ 48 Cores (Milan, 3rd gen)                            |  2021-Q1      | 23ns and 107ns
AMD Ryzen 9 5950X @ 3.40 GHz 16 Cores (Zen3, 4th gen)                |  2020-Q4      | 17ns and 85ns
AWS Graviton3 @ 64 Cores (Arm Neoverse, 3rd gen)                     |  2021-Q4      | 46ns
AWS Graviton2 @ 64 Cores (Arm Neoverse, 2rd gen)                     |  2020-Q1      | 47ns
Apple M1                                                             |  2020-Q4      | 39ns
Apple M1 Max                                                         |  2021-Q4      | 39ns

**See the notebook for additional CPU graphs: [results/results.ipynb](results/results.ipynb), it includes hyperthreading and dual socket configurations**

### Intel Xeon Platinum 8375C @ 2.90GHz 32 Cores (Ice Lake, 3rd gen) 2021-Q2

From an AWS `c6i.metal` machine.

<img src="https://user-images.githubusercontent.com/297060/190918865-7eaae192-6da6-41db-8faf-9496f6a7754b.png" width="1000" />

### Intel Xeon Platinum 8275CL @ 3.00GHz 24 Cores (Cascade Lake, 2nd gen) 2019-Q2

From an AWS `c5.metal` machine.

<img src="https://user-images.githubusercontent.com/297060/190918895-8b90cc12-2e72-41d1-808b-6f03a8771898.png" width="700" />

### Intel Core i9-9900K @ 3.60 GHz 8 Cores (Coffee Lake, 8th gen) 2018-Q4

My gaming machine, it's twice as fast as the other server-oriented CPUs.

<img src="https://user-images.githubusercontent.com/297060/190918912-8b551b33-14e6-4cd3-a82d-8ac241d1abb6.png" width="400" />

### Intel Xeon E5-2695 v4 @ 2.10GHz 18 Cores (Broadwell, 5th gen) 2016-Q1

From a machine provided by GTHost

<img src="https://user-images.githubusercontent.com/297060/190918934-a2b11676-e6e1-4b88-a8e4-6bf69663d477.png" width="550" />

### Intel Core i5-4590 CPU @ 3.30GHz 4 Cores (Haswell, 4th gen) 2014-Q2

Data provided by [Felipe Lube de Bragança](https://github.com/felubra)

<img src="https://user-images.githubusercontent.com/297060/190928985-42e13598-f5dc-4b49-b67e-dc300207d3c7.png" width="450" />

### AMD EPYC 7R13 @ 48 Cores (Milan, 3rd gen) 2021-Q1

From an AWS `c6a.metal` machine.

We can see cores arranged in 6 groups of 8 in which latency is excellent within
(23ns). When data crosses groups, the latency jumps to around 110ns. Note, that
the last 3 groups have a better cross-group latency than the first 3 (~90ns).

<img src="https://user-images.githubusercontent.com/297060/190893255-56ea9890-9e06-4f2d-bcef-249a70c4597b.png" width="1000" />

### AMD Ryzen 9 5950X @ 3.40 GHz 16 Cores (Zen3, 4th gen) 2020-Q1

Data provided by [John Schoenick](https://github.com/Nephyrin)

We can see 2 groups of 8 cores with latencies of 17ns intra-group, and 85ns inter-group.

<img src="https://user-images.githubusercontent.com/297060/190926938-400092a0-45ff-4a6c-816a-1b694767c993.png" width="530" />

### AWS Graviton3 @ 64 Cores (Arm Neoverse, 3rd gen) 2021-Q4

From an AWS `c7g.16xlarge` machine.

<img src="https://user-images.githubusercontent.com/297060/190919040-7d6d2283-cbef-4544-8b07-f93f71754343.png" width="1000" />

### AWS Graviton2 @ 64 Cores (Arm Neoverse, 2nd gen) 2020-Q1

From an AWS `c6gd.metal` machine.

<img src="https://user-images.githubusercontent.com/297060/190919053-11480075-6731-49ce-af03-f50bb27e8b33.png" width="1000" />


**See the notebook for additional CPU graphs: [results/results.ipynb](results/results.ipynb), it includes hyperthreading and dual socket configurations**

How to use
----------

First [install Rust](https://www.rust-lang.org/tools/install) and `gcc` on linux, then:

```
$ cargo install core-to-core-latency
$ core-to-core-latency
Num cores: 10
Using RDTSC to measure time: false
Num round trips per samples: 1000
Num samples: 300
Showing latency=round-trip-time/2 in nanoseconds:

       0       1       2       3       4       5       6       7       8       9
  0
  1   52±6
  2   38±6    39±4
  3   39±5    39±6    38±6
  4   34±6    38±4    37±6    36±5
  5   38±5    38±6    38±6    38±6    37±6
  6   38±5    37±6    39±6    36±4    49±6    38±6
  7   36±6    39±5    39±6    37±6    35±6    36±6    38±6
  8   37±5    38±6    35±5    39±5    38±6    38±5    37±6    37±6
  9   48±6    39±6    36±6    39±6    38±6    36±6    41±6    38±6    39±6

Min  latency: 34.5ns ±6.1 cores: (4,0)
Max  latency: 52.1ns ±9.4 cores: (1,0)
Mean latency: 38.4ns
```

Contribute
-----------

Use `core-to-core-latency 5000 --csv > output.csv` to instruct the program to use
5000 iterations per sample to reduce the noise, and save the results.

It can be used in the jupter notebook [results/results.ipynb](results/results.ipynb) for rendering graphs.

Create a GitHub issue with the generated `output.csv` file and I'll add your results.

License
-------

This software is licensed under the MIT license
