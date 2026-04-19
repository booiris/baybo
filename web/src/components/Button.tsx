import type { ButtonHTMLAttributes, ReactNode } from 'react';

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: 'primary' | 'default';
  children: ReactNode;
}

const base =
  'inline-flex items-center gap-2 px-5 py-2.5 font-bold text-[0.95rem] uppercase tracking-wider border-[3px] border-black rounded-md shadow-brutal-sm cursor-pointer transition-[transform,box-shadow] duration-100 active:translate-x-[3px] active:translate-y-[3px] active:shadow-none disabled:opacity-50 disabled:cursor-not-allowed disabled:shadow-none disabled:!translate-x-0 disabled:!translate-y-0';

const variants: Record<NonNullable<ButtonProps['variant']>, string> = {
  primary: 'bg-brand text-white hover:bg-brand-hover',
  default: 'bg-white text-ink hover:bg-gray-50',
};

export function Button({ variant = 'default', className = '', children, ...rest }: ButtonProps) {
  return (
    <button className={`${base} ${variants[variant]} ${className}`} {...rest}>
      {children}
    </button>
  );
}
