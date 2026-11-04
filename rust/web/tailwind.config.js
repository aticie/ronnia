/** @type {import('tailwindcss').Config} */
export default {
  content: [
    "./index.html",
    "./src/**/*.{vue,js,ts,jsx,tsx}",
  ],
  theme: {
    extend: {
      colors: {
        red: {
          DEFAULT: "#da0037",
        }
      },
      animation: {
        "scroll": "scroll var(--duration) linear infinite"
      },
      keyframes: {
        'scroll': {
          '0%': { transform: `translateY(0px)` },
          '100%': { transform: `translateY(var(--row))` }
        }
      }
    },
  },
  plugins: [],
}
