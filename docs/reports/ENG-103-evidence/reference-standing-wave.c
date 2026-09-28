// Standalone Basilisk twin of `grid-fluid-scenario --scenario standing-wave`;
// not compiled into or linked with Spall. Build: qcc -O2 reference-standing-wave.c -o ref-wave -lm
// Run: ./ref-wave [level]   (level 5 => h = 0.25 m, level 6 => h = 0.125 m)
#include "grid/octree.h"
#include "embed.h"
#include "navier-stokes/centered.h"
#include "two-phase.h"
#include "reduced.h"

u.n[embed] = dirichlet(0.);
u.t[embed] = neumann(0.);
u.r[embed] = neumann(0.);
p[top] = dirichlet(0.);
pf[top] = dirichlet(0.);
u.n[top] = neumann(0.);
f[top] = dirichlet(0.);

int level = 5;
const double L = 6., H = 1., A = 0.1;

int main(int argc, char ** argv) {
  if (argc > 1) level = atoi(argv[1]);
  size(8.);
  init_grid(1 << level);
  rho1 = 1000.; rho2 = 1.2;
  mu1 = mu2 = 0.;
  G.y = -9.81;
  DT = 1./60.;
  TOLERANCE = 1e-8;
  NITERMAX = 400;
  run();
}

static int fluid_space(double x, double y, double z) {
  return x >= 0. && x < L && y >= 0. && z >= 0. && z < 0.5;
}

event init(t = 0) {
  mask(y >= 3. ? top : none);
  foreach() cs[] = fluid_space(x,y,z);
  foreach_face(x) fs.x[] = fluid_space(x - Delta/2.,y,z) && fluid_space(x + Delta/2.,y,z);
  foreach_face(y) fs.y[] = fluid_space(x,y - Delta/2.,z) && fluid_space(x,y + Delta/2.,z);
  foreach_face(z) fs.z[] = fluid_space(x,y,z - Delta/2.) && fluid_space(x,y,z + Delta/2.);
  boundary({cs,fs});
  const double k = pi/L;
  foreach() {
    // Same exact column-average cosine surface as the Spall runner.
    double x0 = x - Delta/2., x1 = x + Delta/2.;
    double eta = A*(sin(k*x1) - sin(k*x0))/(k*Delta);
    f[] = fluid_space(x,y,z) ? clamp((H + eta - (y - Delta/2.))/Delta, 0., 1.) : 0.;
  }
}

// End-column elevations, antisymmetric half difference, every tick.
event probe(i++; t <= 30.) {
  double left = 0., right = 0., kinetic = 0., wleft = 0., wright = 0.;
  foreach(reduction(+:left) reduction(+:right) reduction(+:wleft) reduction(+:wright)
          reduction(+:kinetic)) {
    if (x < Delta && z < 0.5) { left += f[]*Delta*Delta; if (y < Delta) wleft += Delta; }
    if (x > L - Delta && x < L && z < 0.5) { right += f[]*Delta*Delta; if (y < Delta) wright += Delta; }
    kinetic += .5*rho1*f[]*dv()*(sq(u.x[]) + sq(u.y[]) + sq(u.z[]));
  }
  // left/right are column area integrals over z; divide by probe width.
  double hl = wleft > 0. ? left/wleft : 0., hr = wright > 0. ? right/wright : 0.;
  printf("{\"t\":%.9g,\"end_elevation_m\":%.9g,\"kinetic_energy_j\":%.9g}\n",
         t, 0.5*(hl - hr), kinetic);
  fflush(stdout);
}

event stop(t = 30.) { return 1; }
